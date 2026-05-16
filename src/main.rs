mod state;
mod trace;

// sway-tab — History-aware Alt+Tab for Sway WM
// Tracks a global window history via focus events and cycles through all
// recently focused windows when Alt+Tab is pressed, rolling around
// circularly.
//
// Commit happens in two ways:
//   1. Lazy: a focus event arrives that we didn't cause (user clicked,
//      used a different shortcut, etc.)
//   2. Timeout: no Alt+Tab press or release arrives within --commit-timeout
//      seconds, so the selection is committed automatically.
//
// Signal assignments:
//   SIGUSR1  — Alt+Tab pressed (advance cycle, reset timer)
//   SIGRTMIN — --release Alt+Tab (Tab released while Alt held, reset timer)

use anyhow::Result;
use clap::Parser;
use futures_lite::StreamExt;
use state::{FocusAction, State};
use std::sync::Arc;
use std::time::Duration;
use swayipc_async::{Connection, Event, EventType, WindowChange};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Mutex};

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
/// Returns true if the cycle was advanced (so the caller can reset the timer).
async fn advance_cycle(state: &Arc<Mutex<State>>) -> Result<bool> {
    let mut s = state.lock().await;
    let target = match s.advance_cycle() {
        Some(t) => t,
        None => return Ok(false),
    };
    drop(s);

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

    Ok(true)
}

/// Commit the currently previewed window and unfreeze history.
async fn commit_cycle(state: &Arc<Mutex<State>>) {
    let mut s = state.lock().await;
    s.commit_cycle();
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

    // --release Alt+Tab → SIGRTMIN (Tab released while Alt held; resets the commit timer)
    let cmd = format!("bindsym --release Alt+Tab exec kill -RTMIN {pid}");
    tracing::trace!("setup_bindings: setting up {cmd}");
    let results = conn.run_command(&cmd).await?;
    if results.iter().any(|r| r.is_err()) {
        tracing::trace!("setup_bindings: WARNING failed to bind --release Alt+Tab");
        eprintln!("Warning: failed to bind --release Alt+Tab");
    }

    tracing::trace!("setup_bindings: success");
    Ok(())
}

async fn remove_bindings() -> Result<()> {
    tracing::trace!("remove_bindings: called");
    let mut conn = Connection::new().await?;
    for cmd in ["unbindsym Alt+Tab", "unbindsym --release Alt+Tab"] {
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
    trace::init_trace(cli.trace);

    let commit_timeout = Duration::from_secs_f64(cli.commit_timeout);
    tracing::trace!(
        "config: commit_timeout={:.1}s",
        commit_timeout.as_secs_f64()
    );

    let pid = std::process::id();
    setup_bindings(pid).await?;
    tracing::trace!("bindings set up");

    let state = Arc::new(Mutex::new(State::default()));

    // watch channel used to reset the commit timer.
    // Sender sends () each time a tab press or release is received while cycling.
    // The timer task waits for the timeout after each reset; if none arrives in
    // time it commits.
    let (timer_tx, mut timer_rx) = watch::channel(());

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
                    let mut s = state_clone.lock().await;
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
            }
        }
    });

    // SIGUSR1: Alt+Tab pressed — advance cycle and reset commit timer.
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let state_adv = state.clone();
    let timer_tx_adv = timer_tx.clone();
    let advance_task = tokio::spawn(async move {
        while sigusr1.recv().await.is_some() {
            tracing::trace!("advance_task: SIGUSR1 received");
            match advance_cycle(&state_adv).await {
                Ok(true) => {
                    tracing::trace!("advance_task: resetting commit timer");
                    let _ = timer_tx_adv.send(());
                }
                Ok(false) => {}
                Err(e) => eprintln!("Advance error: {e}"),
            }
        }
    });

    // SIGRTMIN: --release Alt+Tab — Tab released while Alt held; reset commit timer.
    // SIGRTMIN = signal 34, the lowest realtime signal available for user use.
    let mut sigrtmin = signal(SignalKind::from_raw(34))?;
    let timer_tx_rel = timer_tx.clone();
    let release_task = tokio::spawn(async move {
        while sigrtmin.recv().await.is_some() {
            tracing::trace!("release_task: SIGRTMIN received (Alt+Tab released), resetting commit timer");
            let _ = timer_tx_rel.send(());
        }
    });

    // Commit timer: after each reset signal, wait commit_timeout. If no further
    // reset arrives before the deadline, commit the current selection.
    let state_timer = state.clone();
    let timer_task = tokio::spawn(async move {
        loop {
            // Outer loop: wait for the first activation (first Alt+Tab press).
            if timer_rx.changed().await.is_err() {
                break;
            }
            tracing::trace!(
                "timer_task: activated, waiting {:.1}s",
                commit_timeout.as_secs_f64()
            );
            // Inner loop: keep restarting the sleep on each reset until it
            // expires. This ensures the last Tab press always starts a fresh
            // sleep even if no further presses follow.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(commit_timeout) => {
                        tracing::trace!("timer_task: timeout expired, committing");
                        commit_cycle(&state_timer).await;
                        break; // back to outer loop, await next activation
                    }
                    result = timer_rx.changed() => {
                        if result.is_err() {
                            return;
                        }
                        tracing::trace!(
                            "timer_task: reset received, restarting {:.1}s timer",
                            commit_timeout.as_secs_f64()
                        );
                        // Sleep will restart on next inner-loop iteration.
                    }
                }
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
    release_task.abort();
    timer_task.abort();
    event_task.abort();

    Ok(())
}
