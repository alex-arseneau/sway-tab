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
use state::{FocusAction, State};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use swayipc::{Connection, Event, EventType, Node, NodeType, WindowChange};

const SIGRTMIN: i32 = 34;

fn print_usage() {
    eprintln!("Usage: sway-tab [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --trace          Enable verbose trace logging to stderr");
    eprintln!("  --timeout N, -t N  Commit timeout in seconds (default: 10.0)");
    eprintln!("  --help, -h       Print this help and exit");
}

fn parse_args() -> (bool, f64) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut trace = false;
    let mut timeout = 4.0_f64;
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

fn init_trace(trace: bool) {
    if trace {
        tracing_subscriber::fmt::Subscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::stderr)
            .init();
    }
}

/// Collect all leaf window con_ids from the sway tree.
/// Walks recursively through nodes and floating_nodes, collecting
/// Con and FloatingCon nodes that have no child nodes (leaf windows).
fn collect_window_ids(node: &Node, out: &mut Vec<i64>) {
    match node.node_type {
        NodeType::Con | NodeType::FloatingCon => {
            if node.nodes.is_empty() && node.floating_nodes.is_empty() {
                // Leaf container — this is a window.
                out.push(node.id);
                return;
            }
        }
        _ => {}
    }
    for child in &node.nodes {
        collect_window_ids(child, out);
    }
    for child in &node.floating_nodes {
        collect_window_ids(child, out);
    }
}

/// Query the sway tree and return all window con_ids.
/// The focused window (if any) is placed last so it ends up at the
/// front of history after seeding.
fn get_all_windows() -> Result<Vec<i64>, Box<dyn std::error::Error>> {
    let mut conn = Connection::new()?;
    let tree = conn.get_tree()?;
    let mut ids = Vec::new();
    collect_window_ids(&tree, &mut ids);

    // Find the currently focused window and move it to the end
    // so that seed() places it at position 0 (most recent).
    fn find_focused(node: &Node) -> Option<i64> {
        if node.focused {
            return Some(node.id);
        }
        for child in &node.nodes {
            if let Some(id) = find_focused(child) {
                return Some(id);
            }
        }
        for child in &node.floating_nodes {
            if let Some(id) = find_focused(child) {
                return Some(id);
            }
        }
        None
    }

    if let Some(focused_id) = find_focused(&tree) {
        if let Some(pos) = ids.iter().position(|&id| id == focused_id) {
            ids.remove(pos);
            ids.push(focused_id);
        }
    }

    tracing::trace!("get_all_windows: found {} windows", ids.len());
    Ok(ids)
}

fn setup_bindings(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    tracing::trace!("setup_bindings: pid={pid}");
    let mut conn = Connection::new()?;

    // Alt+Tab → SIGUSR1 (advance cycle)
    let cmd = format!("bindsym Alt+Tab exec kill -USR1 {pid}");
    tracing::trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd)?;
    if results.iter().any(|r| r.is_err()) {
        tracing::trace!("setup_bindings: FAILED to bind Alt+Tab");
        return Err("Failed to bind Alt+Tab".into());
    }

    // --release Alt+Tab → SIGRTMIN (Tab released while Alt held; resets the commit timer)
    let cmd = format!("bindsym --release Alt+Tab exec kill -RTMIN {pid}");
    tracing::trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd)?;
    if results.iter().any(|r| r.is_err()) {
        tracing::trace!("setup_bindings: WARNING failed to bind --release Alt+Tab");
        eprintln!("Warning: failed to bind --release Alt+Tab");
    }

    tracing::trace!("setup_bindings: success");
    Ok(())
}

fn remove_bindings() {
    tracing::trace!("remove_bindings: called");
    let mut conn = match Connection::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::trace!("remove_bindings: connection FAILED: {e}");
            eprintln!("Warning: failed to connect for unbinding: {e}");
            return;
        }
    };
    for cmd in ["unbindsym Alt+Tab", "unbindsym --release Alt+Tab"] {
        tracing::trace!("remove_bindings: {cmd}");
        match conn.run_command(cmd) {
            Ok(results) => {
                if results.iter().any(|r| r.is_err()) {
                    tracing::trace!("remove_bindings: WARNING failed to unbind {cmd}");
                    eprintln!("Warning: failed to unbind: {cmd}");
                }
            }
            Err(e) => {
                tracing::trace!("remove_bindings: command FAILED: {e}");
                eprintln!("Warning: unbind command failed: {e}");
            }
        }
    }
    tracing::trace!("remove_bindings: success");
}

fn main() {
    let (trace, timeout_secs) = parse_args();
    init_trace(trace);

    let commit_timeout = Duration::from_secs_f64(timeout_secs);
    tracing::trace!(
        "config: commit_timeout={:.1}s",
        commit_timeout.as_secs_f64()
    );

    let pid = std::process::id();
    if let Err(e) = setup_bindings(pid) {
        eprintln!("Failed to set up bindings: {e}");
        std::process::exit(1);
    }
    tracing::trace!("bindings set up");

    let state = Arc::new(Mutex::new(State::default()));

    // Seed history with all existing windows so alt-tab works immediately.
    match get_all_windows() {
        Ok(ids) => {
            let mut s = state.lock().unwrap();
            s.seed(&ids);
            tracing::trace!("seeded history with {} windows", ids.len());
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
        tracing::trace!("event loop started");
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
                        let action = s.handle_focus_event(con_id);
                        match action {
                            FocusAction::Tracked => {
                                tracing::trace!("event: con_id={con_id} tracked");
                            }
                            FocusAction::Ignored => {}
                            FocusAction::AutoCommitted => {
                                tracing::trace!("event: con_id={con_id} auto-committed");
                            }
                        }
                    }
                    WindowChange::Close => {
                        tracing::trace!("event: window closed con_id={con_id}");
                        let mut s = state_event.lock().unwrap();
                        s.handle_close_event(con_id);
                    }
                    _ => {}
                }
            }
        }
    });

    // Timer thread: waits for activation via condvar, then runs commit_timeout.
    let state_timer = state.clone();
    let timer_pair_timer = timer_pair.clone();
    thread::spawn(move || {
        loop {
            // Outer loop: wait for activation (first signal sets reset_flag).
            {
                let (lock, cvar) = &*timer_pair_timer;
                let mut flag = lock.lock().unwrap();
                while !*flag {
                    flag = cvar.wait(flag).unwrap();
                }
                *flag = false;
            }

            tracing::trace!(
                "timer_task: activated, waiting {:.1}s",
                commit_timeout.as_secs_f64()
            );

            // Inner loop: wait_timeout, restart on reset.
            loop {
                let (lock, cvar) = &*timer_pair_timer;
                let flag = lock.lock().unwrap();
                let (mut flag, timeout_result) =
                    cvar.wait_timeout(flag, commit_timeout).unwrap();

                if *flag {
                    // Reset requested — restart timer.
                    *flag = false;
                    tracing::trace!(
                        "timer_task: reset received, restarting {:.1}s timer",
                        commit_timeout.as_secs_f64()
                    );
                    continue;
                }

                if timeout_result.timed_out() {
                    // Timeout expired — commit.
                    tracing::trace!("timer_task: timeout expired, committing");
                    let mut s = state_timer.lock().unwrap();
                    s.commit_cycle();
                    break; // back to outer loop
                }

                // Spurious wakeup — keep waiting (flag was false and no timeout).
                // This shouldn't normally happen, but handle gracefully.
            }
        }
    });

    // Signal loop on main thread.
    let mut signals =
        Signals::new([SIGUSR1, SIGRTMIN, SIGTERM, SIGINT]).expect("Failed to register signals");

    for sig in signals.forever() {
        match sig {
            SIGUSR1 => {
                tracing::trace!("signal_loop: SIGUSR1 received");
                let mut s = state.lock().unwrap();
                let target = s.advance_cycle();
                drop(s);

                if let Some(target) = target {
                    // Focus the target window.
                    match Connection::new() {
                        Ok(mut conn) => {
                            let cmd = format!("[con_id={target}] focus");
                            tracing::trace!("focus: con_id={target} cmd={cmd}");
                            match conn.run_command(&cmd) {
                                Ok(results) => {
                                    if results.iter().any(|r| r.is_err()) {
                                        tracing::trace!(
                                            "focus: FAILED for con_id={target}"
                                        );
                                        eprintln!("Preview focus error for con_id={target}");
                                    } else {
                                        tracing::trace!("focus: success con_id={target}");
                                    }
                                }
                                Err(e) => {
                                    tracing::trace!("focus: command FAILED: {e}");
                                    eprintln!("Preview focus error: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::trace!("focus: connection FAILED: {e}");
                            eprintln!("Connection error: {e}");
                        }
                    }

                    // Notify timer (set flag + notify).
                    let (lock, cvar) = &*timer_pair;
                    let mut flag = lock.lock().unwrap();
                    *flag = true;
                    cvar.notify_one();
                    tracing::trace!("signal_loop: resetting commit timer");
                }
            }
            SIGRTMIN => {
                tracing::trace!(
                    "signal_loop: SIGRTMIN received (Alt+Tab released), resetting commit timer"
                );
                // Just reset the timer.
                let (lock, cvar) = &*timer_pair;
                let mut flag = lock.lock().unwrap();
                *flag = true;
                cvar.notify_one();
            }
            SIGTERM => {
                tracing::trace!("shutdown signal: SIGTERM");
                break;
            }
            SIGINT => {
                tracing::trace!("shutdown signal: SIGINT");
                break;
            }
            _ => {}
        }
    }

    eprintln!("Shutting down...");
    remove_bindings();
    tracing::trace!("bindings removed");
}
