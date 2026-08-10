pub mod actions;
pub mod assets;
pub mod diagnostics;
pub mod http;
pub mod logo;
mod piku_app;

pub use piku_app::run;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Install the tracing subscriber.
///
/// The default filter stays `piku=info` on stderr — unchanged, and what a
/// normal run sees.
///
/// Setting `PIKU_TRACE_SPANS=1` additionally emits an event when each span
/// closes, which carries the span's own duration. That is how render-pass and
/// backend-operation latency get measured:
///
/// ```text
/// PIKU_TRACE_SPANS=1 RUST_LOG=piku=info,piku::render=trace cargo run
/// ```
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("piku=info"));
    let span_events = if diagnostics::trace_spans_enabled() {
        FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(span_events)
        .with_writer(std::io::stderr)
        .init();
}
