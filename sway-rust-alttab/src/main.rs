// sway-rust-alttab — History-aware Alt+Tab for Sway WM
// Tracks a global window history via focus events and cycles through all
// recently focused windows when Alt+Tab is pressed, rolling around
// circularly. Uses SIGUSR1 for key presses and a timeout for commitment.

use anyhow::Result;
use futures_lite::StreamExt;
use std::collections::VecDeque;
use swayipc_async::{Connection, Event, EventType, WindowChange};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use std::sync::Arc;

/// Persistent history of recently focused windows, populated by focus events.
/// Deduplicates and maintains recency order so Alt+Tab cycles through
/// windows in the order the user last visited them.
#[derive(Default)]
struct WindowHistory {
    /// Ordered from most-recent (index 0) to least-recent.
    /// Index 0 = the window you last focused before the current one,
    /// which is where the first Alt+Tab should go.
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

/// Shared state between the focus-event loop and the SIGUSR1 handler.
struct State {
    history: WindowHistory,
    /// Some(_) when the user is actively cycling. The usize is the
    /// index into `history.history`.
    cycle_pos: Option<usize>,
    /// Timeout task handle — cancelled when a new Tab press arrives.
    pending_commit: Option<tokio::task::JoinHandle<()>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: WindowHistory::new(15),
            cycle_pos: None,
            pending_commit: None,
        }
    }
}

/// How long to wait after the last Alt+Tab press before committing.
const COMMIT_TIMEOUT_MS: u64 = 300;

/// Focus the given container via sway IPC.
async fn focus_con_id(conn: &mut Connection, con_id: i64) -> Result<()> {
    let cmd = format!("[con_id={con_id}] focus");
    let results = conn.run_command(&cmd).await?;
    if results.iter().any(|r| r.is_err()) {
        anyhow::bail!("focus command failed");
    }
    Ok(())
}

/// Advance the Alt+Tab cycle by one and schedule a commit timeout.
async fn handle_sigusr1(state: &Arc<Mutex<State>>) -> Result<()> {
    let mut s = state.lock().await;

    if s.history.len() < 2 {
        return Ok(());
    }

    // Cancel any previous pending commit — user pressed again before timeout.
    if let Some(handle) = s.pending_commit.take() {
        handle.abort();
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

    // Schedule commit after COMMIT_TIMEOUT_MS.
    let state_clone = state.clone();
    let commit_task = tokio::spawn(async move {
        sleep(Duration::from_millis(COMMIT_TIMEOUT_MS)).await;
        // Reacquire and finalize the cycle.
        let mut s = state_clone.lock().await;
        if let Some(pos) = s.cycle_pos.take() {
            s.history.promote(pos);
            s.history.frozen = false;
        }
        s.pending_commit = None;
    });

    // Store the handle so a subsequent Tab press can cancel it.
    {
        let mut s = state.lock().await;
        s.pending_commit = Some(commit_task);
    }

    Ok(())
}

async fn setup_binding(pid: u32) -> Result<()> {
    let mut conn = Connection::new().await?;
    let results = conn
        .run_command(&format!("bindsym Alt+Tab exec kill -USR1 {pid}"))
        .await?;
    if results.iter().any(|r| r.is_err()) {
        anyhow::bail!("Failed to bind Alt+Tab");
    }
    Ok(())
}

async fn remove_binding() -> Result<()> {
    let mut conn = Connection::new().await?;
    let results = conn.run_command("unbindsym Alt+Tab").await?;
    if results.iter().any(|r| r.is_err()) {
        eprintln!("Warning: failed to unbind Alt+Tab");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let pid = std::process::id();
    setup_binding(pid).await?;

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
                    // When frozen, we intentionally ignore focus events
                    // generated by our own preview focusing.
                }
            }
        }
    });

    // SIGUSR1 loop: one signal = one Alt+Tab press.
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let sigusr1_task = tokio::spawn(async move {
        while sigusr1.recv().await.is_some() {
            if let Err(e) = handle_sigusr1(&state).await {
                eprintln!("Toggle error: {e}");
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
    let _ = remove_binding().await;
    event_task.abort();
    sigusr1_task.abort();

    Ok(())
}
