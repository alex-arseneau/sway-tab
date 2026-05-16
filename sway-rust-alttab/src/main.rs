use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use anyhow::Result;
use swayipc_async::{Connection, Event, EventType, WindowChange};
use futures_lite::StreamExt;

#[derive(Default)]
struct State {
    current: Option<i64>,
    previous: Option<i64>,
}

async fn sway_event_loop(mut events: swayipc_async::EventStream, state: Arc<Mutex<State>>) -> Result<()> {
    while let Some(event) = events.next().await {
        let event = event?;
        if let Event::Window(window_event) = event {
            if window_event.change == WindowChange::Focus {
                let mut s = state.lock().await;
                let id = window_event.container.id;
                if s.current != Some(id) {
                    s.previous = s.current;
                    s.current = Some(id);
                }
            }
        }
    }
    Ok(())
}

async fn handle_toggle(state: Arc<Mutex<State>>) -> Result<()> {
    let mut s = state.lock().await;
    let target = s.previous;
    let current = s.current;
    let Some(con_id) = target else {
        return Ok(());
    };
    let mut conn = Connection::new().await?;
    let results = conn.run_command(format!("[con_id={}] focus", con_id)).await?;
    if results.iter().all(|r| r.is_ok()) {
        s.previous = current;
        s.current = target;
    } else {
        s.previous = None;
    }
    Ok(())
}

async fn setup_binding(pid: u32) -> Result<()> {
    let mut conn = Connection::new().await?;
    let results = conn.run_command(format!("bindsym Alt+Tab exec kill -USR1 {}", pid)).await?;
    if !results.iter().all(|r| r.is_ok()) {
        return Err(anyhow::anyhow!("Failed to bind Alt+Tab"));
    }
    Ok(())
}

async fn remove_binding() -> Result<()> {
    let mut conn = Connection::new().await?;
    let results = conn.run_command("unbindsym Alt+Tab").await?;
    if !results.iter().all(|r| r.is_ok()) {
        eprintln!("Warning: failed to unbind Alt+Tab");
    }
    Ok(())
}

async fn sigusr1_loop(state: Arc<Mutex<State>>) -> Result<()> {
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    loop {
        if sigusr1.recv().await.is_none() {
            break;
        }
        if let Err(e) = handle_toggle(state.clone()).await {
            eprintln!("Toggle error: {}", e);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let pid = std::process::id();
    setup_binding(pid).await?;

    let state = Arc::new(Mutex::new(State::default()));

    let events_conn = Connection::new().await?;
    let events = events_conn.subscribe(&[EventType::Window]).await?;
    let state_clone = state.clone();
    let event_task = tokio::spawn(async move {
        if let Err(e) = sway_event_loop(events, state_clone).await {
            eprintln!("Event loop error: {}", e);
        }
    });

    let sigusr1_task = tokio::spawn(async move {
        if let Err(e) = sigusr1_loop(state).await {
            eprintln!("SIGUSR1 loop error: {}", e);
        }
    });

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {},
        _ = sigint.recv() => {},
    }

    eprintln!("Shutting down...");
    if let Err(e) = remove_binding().await {
        eprintln!("Failed to remove binding: {}", e);
    }

    event_task.abort();
    sigusr1_task.abort();

    Ok(())
}
