/// CLI argument parsing and conditional trace logging.
use tracing_subscriber::fmt;

#[derive(Debug, Clone, clap::Parser)]
#[command(name = "sway-tab", about = "Alt-Tab daemon for Sway WM")]
pub struct Cli {
    /// Enable verbose trace logging
    #[arg(long)]
    pub trace: bool,

    /// Seconds of inactivity after the last Alt+Tab before the selection is
    /// committed automatically. Resets each time Tab is pressed or released
    /// while Alt is held. Default: 10.0
    #[arg(long = "timeout", short = 't', default_value_t = 10.0)]
    pub commit_timeout: f64,
}

/// Initialize tracing if `trace` is true. When false, tracing calls are a
/// zero-cost no-op because no subscriber is configured.
pub fn init_trace(trace: bool) {
    if trace {
        fmt::Subscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::stderr)
            .init();
    }
}
