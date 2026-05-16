// sway-rust-alttab — History-aware Alt+Tab for Sway WM
// Tracks a global window history via focus events and cycles through all
// recently focused windows when Alt+Tab is pressed, rolling around
// circularly. Commits the selection when Alt is released (SIGRTMIN).

use anyhow::Result;
use futures_lite::StreamExt;
use std::collections::VecDeque;
use swayipc_async::{Connection, Event, EventType, WindowChange};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use std::sync::Arc;

// Signal assignments:
//   SIGUSR1  — Alt+Tab pressed (advance cycle)
//   SIGRTMIN — Alt key released (commit selection)

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
        self.history.retain(|&id| id != con_id);
        self.history.push_front(con_id);
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
        if let Some(con_id) = self.history.remove(pos) {
            self.history.push_front(con_id);
        }
    }
}

/// Shared state between the focus-event loop and the signal handler.
struct State {
    history: WindowHistory,
    /// Some(_) when the user is actively cycling.
    cycle_pos: Option<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: WindowHistory::new(15),
            cycle_pos: None,
        }
    }
}

/// Focus the given container via sway IPC.
async fn focus_con_id(conn: &mut Connection, con_id: i64) -> Result<()> {
    let cmd = format!("[con_id={con_id}] focus");
    let results = conn.run_command(&cmd).await?;
    if results.iter().any(|r| r.is_err()) {
        anyhow::bail!("focus command failed");
    }
    Ok(())
}

/// Advance the Alt+Tab cycle by one and preview the target window.
async fn advance_cycle(state: &Arc<Mutex<State>>) -> Result<()> {
    let mut s = state.lock().await;

    if s.history.len() < 2 {
        return Ok(());
    }

    if s.cycle_pos.is_none() {
        // First press: freeze history so preview focuses don't pollute it.
        s.history.frozen = true;
        // Position 1 is the previously-focused window (pos 0 is current).
        s.cycle_pos = Some(1);
    } else {
        // Subsequent press: advance circularly through all history positions.
        let pos = s.cycle_pos.unwrap();
        let next_pos = (pos + 1) % s.history.len();
        s.cycle_pos = Some(next_pos);
    }

    let target = s.history.get(s.cycle_pos.unwrap()).unwrap();
    drop(s);

    // Preview: focus the target window.
    match Connection::new().await {
        Ok(mut conn) => {
            if let Err(e) = focus_con_id(&mut conn, target).await {
                eprintln!("Preview focus error: {e}");
            }
        }
        Err(e) => eprintln!("Connection error: {e}"),
    }

    Ok(())
}

/// Commit the currently previewed window and unfreeze history.
async fn commit_cycle(state: &Arc<Mutex<State>>) -> Result<()> {
    let mut s = state.lock().await;
    if let Some(pos) = s.cycle_pos.take() {
        s.history.promote(pos);
        s.history.frozen = false;
    }
    Ok(())
}

async fn setup_bindings(pid: u32) -> Result<()> {
    let mut conn = Connection::new().await?;
    // Alt+Tab → SIGUSR1 (advance cycle)
    let results = conn
        .run_command(&format!("bindsym Alt+Tab exec kill -USR1 {pid}"))
        .await?;
    if results.iter().any(|r| r.is_err()) {
        anyhow::bail!("Failed to bind Alt+Tab");
    }

    // Alt_L release and Alt_R release → SIGRTMIN (commit cycle)
    let cmds = [
        format!("bindsym --release Alt_L exec kill -RTMIN {pid}"),
        format!("bindsym --release Alt_R exec kill -RTMIN {pid}"),
    ];
    for cmd in cmds {
        let results = conn.run_command(&cmd).await?;
        if results.iter().any(|r| r.is_err()) {
            eprintln!("Warning: failed to bind Alt release: {cmd}");
        }
    }

    Ok(())
}

async fn remove_bindings() -> Result<()> {
    let mut conn = Connection::new().await?;
    for cmd in [
        "unbindsym Alt+Tab",
        "unbindsym --release Alt_L",
        "unbindsym --release Alt_R",
    ] {
        let results = conn.run_command(cmd).await?;
        if results.iter().any(|r| r.is_err()) {
            eprintln!("Warning: failed to unbind: {cmd}");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let pid = std::process::id();
    setup_bindings(pid).await?;

    let state = Arc::new(Mutex::new(State::default()));

    // Focus-event loop: populate history from real user focus changes.
    let conn = Connection::new().await?;
    let mut events = conn.subscribe(&[EventType::Window]).await?;
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
                    let mut s = state_clone.lock().await;
                    if !s.history.frozen {
                        s.history.add(window_ev.container.id);
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
            if let Err(e) = advance_cycle(&state_adv).await {
                eprintln!("Advance error: {e}");
            }
        }
    });

    // SIGRTMIN: Alt released (commit cycle)
    // SIGRTMIN = signal 34, the lowest realtime signal available for user use.
    let mut sigrtmin = signal(SignalKind::from_raw(34))?;
    let state_cmt = state.clone();
    let commit_task = tokio::spawn(async move {
        while sigrtmin.recv().await.is_some() {
            if let Err(e) = commit_cycle(&state_cmt).await {
                eprintln!("Commit error: {e}");
            }
        }
    });

    // Wait for shutdown signal.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }

    eprintln!("Shutting down...");
    let _ = remove_bindings().await;
    advance_task.abort();
    commit_task.abort();
    event_task.abort();

    Ok(())
}
