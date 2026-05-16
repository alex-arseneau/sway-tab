mod trace;

// sway-tab — History-aware Alt+Tab for Sway WM
// Tracks a global window history via focus events and cycles through all
// recently focused windows when Alt+Tab is pressed, rolling around
// circularly. Commits the selection lazily: when a focus event arrives
// that we didn't cause, we auto-commit the last previewed window and
// record the new focus as the current window.

use anyhow::Result;
use clap::Parser;
use futures_lite::StreamExt;
use std::collections::VecDeque;
use swayipc_async::{Connection, Event, EventType, WindowChange};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use std::sync::Arc;

// Signal assignments:
//   SIGUSR1 — Alt+Tab pressed (advance cycle)
//   Commit is lazy: triggered by any focus event we didn't cause.

/// Persistent history of recently focused windows, populated by focus events.
/// Deduplicates and maintains recency order so Alt+Tab cycles through
/// windows in the order the user last visited them.
#[derive(Default)]
struct WindowHistory {
    /// Ordered from most-recent (index 0) to least-recent.
    /// Index 0 = current window, index 1 = previously focused.
    history: VecDeque<i64>,
    max_len: usize,
    /// True while the user is actively cycling — prevents preview
    /// focus events from polluting the history.
    frozen: bool,
}

impl WindowHistory {
    fn new(max_len: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(max_len),
            max_len,
            frozen: false,
        }
    }

    /// Add con_id to front of history. If already present, move it.
    fn add(&mut self, con_id: i64) {
        tracing::trace!("history.add: con_id={con_id}");
        self.history.retain(|&id| id != con_id);
        self.history.push_front(con_id);
        tracing::trace!("history.add: new len={}", self.history.len());
        while self.history.len() > self.max_len {
            self.history.pop_back();
        }
    }

    fn get(&self, pos: usize) -> Option<i64> {
        self.history.get(pos).copied()
    }

    fn len(&self) -> usize {
        self.history.len()
    }

    /// Move the item at `pos` to the front (used on commit).
    fn promote(&mut self, pos: usize) {
        tracing::trace!("history.promote: pos={pos}");
        if let Some(con_id) = self.history.remove(pos) {
            self.history.push_front(con_id);
            tracing::trace!("history.promote: con_id={con_id} promoted to front");
        }
    }
}

/// Shared state between the focus-event loop and the signal handler.
struct State {
    history: WindowHistory,
    /// Some(_) when the user is actively cycling.
    cycle_pos: Option<usize>,
    /// The con_id we most recently told sway to focus (our own preview).
    /// Used to distinguish our preview focus events from real user switches.
    last_preview: Option<i64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: WindowHistory::new(15),
            cycle_pos: None,
            last_preview: None,
        }
    }
}

/// Focus the given container via sway IPC.
async fn focus_con_id(conn: &mut Connection, con_id: i64) -> Result<()> {
    let cmd = format!("[con_id={con_id}] focus");
    tracing::trace!("focus: con_id={con_id} cmd={cmd}");
    let results = conn.run_command(&cmd).await?;
    if results.iter().any(|r| r.is_err()) {
        tracing::trace!("focus: FAILED for con_id={con_id}");
        anyhow::bail!("focus command failed");
    }
    tracing::trace!("focus: success con_id={con_id}");
    Ok(())
}

/// Advance the Alt+Tab cycle by one and preview the target window.
async fn advance_cycle(state: &Arc<Mutex<State>>) -> Result<()> {
    tracing::trace!("advance_cycle: called");
    let mut s = state.lock().await;

    if s.history.len() < 2 {
        tracing::trace!("advance_cycle: history too short, skipping");
        return Ok(());
    }

    if s.cycle_pos.is_none() {
        // First press: freeze history so preview focuses don't pollute it.
        tracing::trace!("advance_cycle: first press, freezing history, cycle_pos=1");
        s.history.frozen = true;
        // Position 1 is the previously-focused window (pos 0 is current).
        s.cycle_pos = Some(1);
    } else {
        // Subsequent press: advance circularly through all history positions.
        let pos = s.cycle_pos.unwrap();
        let next_pos = (pos + 1) % s.history.len();
        tracing::trace!("advance_cycle: advancing cycle_pos {pos} -> {next_pos}");
        s.cycle_pos = Some(next_pos);
    }

    let target = s.history.get(s.cycle_pos.unwrap()).unwrap();
    tracing::trace!("advance_cycle: target con_id={target} for preview");
    s.last_preview = Some(target);
    drop(s);

    // Preview: focus the target window.
    match Connection::new().await {
        Ok(mut conn) => {
            tracing::trace!("advance_cycle: creating new connection for preview");
            if let Err(e) = focus_con_id(&mut conn, target).await {
                tracing::trace!("advance_cycle: preview focus FAILED: {e}");
                eprintln!("Preview focus error: {e}");
            } else {
                tracing::trace!("advance_cycle: preview focus success");
            }
        }
        Err(e) => {
            tracing::trace!("advance_cycle: connection FAILED: {e}");
            eprintln!("Connection error: {e}");
        }
    }

    Ok(())
}

async fn setup_bindings(pid: u32) -> Result<()> {
    tracing::trace!("setup_bindings: pid={pid}");
    let mut conn = Connection::new().await?;
    // Alt+Tab → SIGUSR1 (advance cycle)
    let cmd = format!("bindsym Alt+Tab exec kill -USR1 {pid}");
    tracing::trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd).await?;
    if results.iter().any(|r| r.is_err()) {
        tracing::trace!("setup_bindings: FAILED to bind Alt+Tab");
        anyhow::bail!("Failed to bind Alt+Tab");
    }

    tracing::trace!("setup_bindings: success");
    Ok(())
}

async fn remove_bindings() -> Result<()> {
    tracing::trace!("remove_bindings: called");
    let mut conn = Connection::new().await?;
    for cmd in ["unbindsym Alt+Tab"] {
        tracing::trace!("remove_bindings: {cmd}");
        let results = conn.run_command(cmd).await?;
        if results.iter().any(|r| r.is_err()) {
            tracing::trace!("remove_bindings: WARNING failed to unbind {cmd}");
            eprintln!("Warning: failed to unbind: {cmd}");
        }
    }
    tracing::trace!("remove_bindings: success");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = trace::Cli::parse();
    tracing::trace!("trace flag: {}", cli.trace);
    trace::init_trace(cli.trace);

    let pid = std::process::id();
    setup_bindings(pid).await?;
    tracing::trace!("bindings set up");

    let state = Arc::new(Mutex::new(State::default()));

    // Focus-event loop: populate history from real user focus changes.
    let conn = Connection::new().await?;
    let mut events = conn.subscribe(&[EventType::Window]).await?;
    tracing::trace!("event loop started");
    let state_clone = state.clone();
    let event_task = tokio::spawn(async move {
        while let Some(event) = events.next().await {
            let ev = match event {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("Event stream error: {e}");
                    break;
                }
            };
            if let Event::Window(window_ev) = ev {
                if window_ev.change == WindowChange::Focus {
                    let con_id = window_ev.container.id;
                    tracing::trace!("event: focus change on con_id={con_id}");
                    let mut s = state_clone.lock().await;
                    if !s.history.frozen {
                        // Normal operation: track the focus.
                        s.history.add(con_id);
                    } else if s.last_preview == Some(con_id) {
                        // This is our own preview focus — ignore it.
                        tracing::trace!("event: focus ignored (our own preview)");
                    } else {
                        // User moved focus to something we didn't select.
                        // Auto-commit: promote the last previewed position and unfreeze.
                        tracing::trace!(
                            "event: external focus change to con_id={con_id}, auto-committing"
                        );
                        if let Some(pos) = s.cycle_pos.take() {
                            s.history.promote(pos);
                            tracing::trace!("event: auto-commit promoted pos={pos}");
                        }
                        s.history.frozen = false;
                        s.last_preview = None;
                        // Record the new window as the most recent.
                        s.history.add(con_id);
                    }
                }
            }
        }
    });

    // SIGUSR1: Alt+Tab pressed (advance cycle)
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let state_adv = state.clone();
    let advance_task = tokio::spawn(async move {
        while sigusr1.recv().await.is_some() {
            tracing::trace!("advance_task: SIGUSR1 received");
            if let Err(e) = advance_cycle(&state_adv).await {
                eprintln!("Advance error: {e}");
            }
        }
    });

    // Wait for shutdown signal.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::trace!("shutdown signal: SIGTERM");
        }
        _ = sigint.recv() => {
            tracing::trace!("shutdown signal: SIGINT");
        }
    }

    eprintln!("Shutting down...");
    let _ = remove_bindings().await;
    tracing::trace!("bindings removed");
    advance_task.abort();
    event_task.abort();

    Ok(())
}
