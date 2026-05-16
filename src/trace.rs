/// CLI argument parsing and conditional trace logging.
use tracing_subscriber::fmt;

#[derive(Debug, Clone, clap::Parser)]
#[command(name = "sway-tab", about = "Alt-Tab daemon for Sway WM")]
pub struct Cli {
    /// Enable verbose trace logging
    #[arg(long)]
    pub trace: bool,
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
