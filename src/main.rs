mod state;

// sway-tab — History-aware Alt+Tab for Sway WM
// Tracks a global window history via focus events and cycles through all
// recently focused windows when Alt+Tab is pressed, rolling around
// circularly.
//
// Commit happens in two ways:
//   1. Lazy: a focus event arrives that we didn't cause (user clicked,
//      used a different shortcut, etc.)
//   2. Timeout: no Alt+Tab press or release arrives within --timeout
//      seconds, so the selection is committed automatically.
//
// Signal assignments:
//   SIGUSR1  — Alt+Tab pressed (advance cycle, reset timer)
//   SIGRTMIN — --release Alt+Tab (Tab released while Alt held, reset timer)

use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;
use state::State;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{Connection, Event, EventType, Node, NodeType, WindowChange};

const SIGRTMIN: i32 = 34;
const DEFAULT_TIMEOUT: f64 = 4.0;

// Tiny replacement for tracing::trace! — a single global flag toggling
// stderr prints when `--trace` is passed on the command line.
pub static TRACE: AtomicBool = AtomicBool::new(false);

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

fn print_usage() {
    eprintln!("Usage: sway-tab [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --trace          Enable verbose trace logging to stderr");
    eprintln!("  --timeout N, -t N  Commit timeout in seconds (default: {DEFAULT_TIMEOUT:.1})");
    eprintln!("  --help, -h       Print this help and exit");
}

fn parse_args() -> (bool, f64) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut trace = false;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--trace" => {
                trace = true;
            }
            "--timeout" | "-t" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --timeout requires a value");
                    std::process::exit(1);
                }
                timeout = match args[i].parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("Error: invalid timeout value: {}", args[i]);
                        std::process::exit(1);
                    }
                };
            }
            other => {
                eprintln!("Error: unknown option: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    (trace, timeout)
}

/// Set the timer's reset flag and wake the timer thread so it pushes its
/// deadline forward.
fn reset_timer(timer_pair: &(Mutex<bool>, Condvar)) {
    let (lock, cvar) = timer_pair;
    let mut flag = lock.lock().unwrap();
    *flag = true;
    cvar.notify_one();
}

/// Connect to sway and focus the given window, reporting any error.
fn focus_window(con_id: i64) {
    let mut conn = match Connection::new() {
        Ok(conn) => conn,
        Err(e) => {
            trace!("focus: connection FAILED: {e}");
            eprintln!("Connection error: {e}");
            return;
        }
    };
    let cmd = format!("[con_id={con_id}] focus");
    trace!("focus: con_id={con_id} cmd={cmd}");
    match conn.run_command(&cmd) {
        Ok(results) => {
            if results.iter().any(|r| r.is_err()) {
                trace!("focus: FAILED for con_id={con_id}");
                eprintln!("Preview focus error for con_id={con_id}");
            } else {
                trace!("focus: success con_id={con_id}");
            }
        }
        Err(e) => {
            trace!("focus: command FAILED: {e}");
            eprintln!("Preview focus error: {e}");
        }
    }
}

/// Query the sway tree and return all window con_ids. The focused window
/// (if any) is placed last so it ends up at the front of history after
/// seeding. A single recursive walk gathers both pieces of information.
fn get_all_windows() -> Result<Vec<i64>, Box<dyn std::error::Error>> {
    /// Walk the tree once, pushing leaf-window con_ids into `ids` and
    /// returning the focused leaf's con_id if one is found.
    fn walk(node: &Node, ids: &mut Vec<i64>) -> Option<i64> {
        let is_leaf = matches!(node.node_type, NodeType::Con | NodeType::FloatingCon)
            && node.nodes.is_empty()
            && node.floating_nodes.is_empty();
        if is_leaf {
            ids.push(node.id);
            return if node.focused { Some(node.id) } else { None };
        }
        let mut focused = None;
        for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
            if let Some(id) = walk(child, ids) {
                focused = Some(id);
            }
        }
        focused
    }

    let mut conn = Connection::new()?;
    let tree = conn.get_tree()?;
    let mut ids = Vec::new();
    let focused = walk(&tree, &mut ids);

    if let Some(focused_id) = focused {
        if let Some(pos) = ids.iter().position(|&id| id == focused_id) {
            ids.remove(pos);
            ids.push(focused_id);
        }
    }

    trace!("get_all_windows: found {} windows", ids.len());
    Ok(ids)
}

fn setup_bindings(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    trace!("setup_bindings: pid={pid}");
    let mut conn = Connection::new()?;

    // Alt+Tab → SIGUSR1 (advance cycle)
    let cmd = format!("bindsym Alt+Tab exec kill -USR1 {pid}");
    trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd)?;
    if results.iter().any(|r| r.is_err()) {
        trace!("setup_bindings: FAILED to bind Alt+Tab");
        return Err("Failed to bind Alt+Tab".into());
    }

    // --release Alt+Tab → SIGRTMIN (Tab released while Alt held; resets the commit timer)
    let cmd = format!("bindsym --release Alt+Tab exec kill -RTMIN {pid}");
    trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd)?;
    if results.iter().any(|r| r.is_err()) {
        trace!("setup_bindings: WARNING failed to bind --release Alt+Tab");
        eprintln!("Warning: failed to bind --release Alt+Tab");
    }

    trace!("setup_bindings: success");
    Ok(())
}

fn remove_bindings() {
    trace!("remove_bindings: called");
    let mut conn = match Connection::new() {
        Ok(c) => c,
        Err(e) => {
            trace!("remove_bindings: connection FAILED: {e}");
            eprintln!("Warning: failed to connect for unbinding: {e}");
            return;
        }
    };
    for cmd in ["unbindsym Alt+Tab", "unbindsym --release Alt+Tab"] {
        trace!("remove_bindings: {cmd}");
        match conn.run_command(cmd) {
            Ok(results) => {
                if results.iter().any(|r| r.is_err()) {
                    trace!("remove_bindings: WARNING failed to unbind {cmd}");
                    eprintln!("Warning: failed to unbind: {cmd}");
                }
            }
            Err(e) => {
                trace!("remove_bindings: command FAILED: {e}");
                eprintln!("Warning: unbind command failed: {e}");
            }
        }
    }
    trace!("remove_bindings: success");
}

fn main() {
    let (trace, timeout_secs) = parse_args();
    if trace {
        TRACE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let commit_timeout = Duration::from_secs_f64(timeout_secs);
    trace!(
        "config: commit_timeout={:.1}s",
        commit_timeout.as_secs_f64()
    );

    let pid = std::process::id();
    if let Err(e) = setup_bindings(pid) {
        eprintln!("Failed to set up bindings: {e}");
        std::process::exit(1);
    }
    trace!("bindings set up");

    let state = Arc::new(Mutex::new(State::default()));

    // Seed history with all existing windows so alt-tab works immediately.
    match get_all_windows() {
        Ok(ids) => {
            let mut s = state.lock().unwrap();
            s.seed(&ids);
            trace!("seeded history with {} windows", ids.len());
        }
        Err(e) => {
            eprintln!("Warning: failed to seed window history: {e}");
            // Not fatal — history will build up from focus events.
        }
    }

    // Condvar-based timer: (reset_flag, condvar)
    let timer_pair = Arc::new((Mutex::new(false), Condvar::new()));

    // Event thread: subscribe to sway window events and populate history.
    let state_event = state.clone();
    thread::spawn(move || {
        let conn = match Connection::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Event thread: connection failed: {e}");
                return;
            }
        };
        let events = match conn.subscribe([EventType::Window]) {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("Event thread: subscribe failed: {e}");
                return;
            }
        };
        trace!("event loop started");
        for event in events {
            let ev = match event {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("Event stream error: {e}");
                    break;
                }
            };
            if let Event::Window(window_ev) = ev {
                let con_id = window_ev.container.id;
                match window_ev.change {
                    WindowChange::Focus => {
                        let mut s = state_event.lock().unwrap();
                        s.handle_focus_event(con_id);
                    }
                    WindowChange::Close => {
                        trace!("event: window closed con_id={con_id}");
                        let mut s = state_event.lock().unwrap();
                        s.handle_close_event(con_id);
                    }
                    _ => {}
                }
            }
        }
    });

    // Timer thread: single loop tracking an Option<Instant> deadline.
    // None = idle (wait indefinitely); Some = wait until deadline, then commit.
    // Each reset flag flip from the signal loop pushes the deadline forward.
    let state_timer = state.clone();
    let timer_pair_timer = timer_pair.clone();
    thread::spawn(move || {
        let (lock, cvar) = &*timer_pair_timer;
        let mut deadline: Option<Instant> = None;
        let mut flag = lock.lock().unwrap();
        loop {
            flag = match deadline {
                None => cvar.wait(flag).unwrap(),
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(remaining) => cvar.wait_timeout(flag, remaining).unwrap().0,
                    None => flag, // already expired — fall through to commit
                },
            };

            if *flag {
                *flag = false;
                deadline = Some(Instant::now() + commit_timeout);
                trace!(
                    "timer_task: reset, waiting {:.1}s",
                    commit_timeout.as_secs_f64()
                );
            } else if let Some(d) = deadline {
                if Instant::now() >= d {
                    trace!("timer_task: timeout expired, committing");
                    deadline = None;
                    // Drop the flag lock while we touch state.
                    drop(flag);
                    state_timer.lock().unwrap().commit_cycle();
                    flag = lock.lock().unwrap();
                }
            }
        }
    });

    // Signal loop on main thread.
    let mut signals =
        Signals::new([SIGUSR1, SIGRTMIN, SIGTERM, SIGINT]).expect("Failed to register signals");

    for sig in signals.forever() {
        match sig {
            SIGUSR1 => {
                trace!("signal_loop: SIGUSR1 received");
                let target = state.lock().unwrap().advance_cycle();
                if let Some(target) = target {
                    focus_window(target);
                    reset_timer(&timer_pair);
                    trace!("signal_loop: resetting commit timer");
                }
            }
            SIGRTMIN => {
                trace!(
                    "signal_loop: SIGRTMIN received (Alt+Tab released), resetting commit timer"
                );
                reset_timer(&timer_pair);
            }
            SIGTERM | SIGINT => {
                trace!("shutdown signal: {sig}");
                break;
            }
            _ => {}
        }
    }

    eprintln!("Shutting down...");
    remove_bindings();
    trace!("bindings removed");
}
