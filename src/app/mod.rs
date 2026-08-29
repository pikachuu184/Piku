pub mod actions;
pub mod assets;
pub mod diagnostics;
pub mod http;
pub mod logo;
mod piku_app;

pub use piku_app::run;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Our own events at `info`, and the renderer stack's at `warn`.
///
/// The GPU targets are what made an atlas panic arrive with no context in front
/// of it. `gpui_wgpu` logs "GPU error during frame (failure N of 10)" and
/// `gpui_linux` logs "GPU recovery failed, will retry on next frame" — through
/// the `log` facade, which `tracing_subscriber::fmt().init()` already bridges
/// (see [`init_tracing`]). The records were reaching the subscriber all along;
/// a filter of `piku=info` names no directive that matches target `gpui_wgpu`,
/// so `EnvFilter` dropped every one. Naming them is the whole fix.
///
/// `wgpu`/`wgpu_hal`/`wgpu_core` are here because a device loss is reported
/// there first, one layer under gpui.
const DEFAULT_FILTER: &str = "piku=info,\
    gpui=warn,gpui_wgpu=warn,gpui_linux=warn,gpui_platform=warn,\
    wgpu=warn,wgpu_hal=warn,wgpu_core=warn";

/// Install the tracing subscriber.
///
/// The default filter is [`DEFAULT_FILTER`] on stderr. `RUST_LOG` replaces it
/// wholesale, so an override that wants renderer diagnostics has to name those
/// targets itself.
///
/// The `log` → `tracing` bridge is not installed here on purpose.
/// `SubscriberBuilder::init` already installs `LogTracer` (`tracing-log` is a
/// default feature of `tracing-subscriber`) and caps the `log` facade at the
/// subscriber's own max-level hint, which is derived from the filter above — so
/// a hand-rolled `LogTracer::init` both duplicates it and makes this function
/// panic on `SetLoggerError`.
///
/// Setting `PIKU_TRACE_SPANS=1` additionally emits an event when each span
/// closes, which carries the span's own duration. That is how render-pass and
/// backend-operation latency get measured:
///
/// ```text
/// PIKU_TRACE_SPANS=1 RUST_LOG=piku=info,piku::render=trace cargo run
/// ```
pub fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed directive would fall back to no filtering at all, and the
    /// line-continuation escapes in [`DEFAULT_FILTER`] are easy to drop in an
    /// edit — which would embed leading whitespace in a target name and silence
    /// that target instead of the loud failure you would want.
    #[test]
    fn the_default_filter_parses_and_has_no_stray_whitespace() {
        assert!(
            !DEFAULT_FILTER.contains(char::is_whitespace),
            "DEFAULT_FILTER has whitespace in it: {DEFAULT_FILTER:?}"
        );
        assert!(
            EnvFilter::try_new(DEFAULT_FILTER).is_ok(),
            "DEFAULT_FILTER is not a valid filter: {DEFAULT_FILTER:?}"
        );
    }

    /// The subscriber's max-level hint is what `SubscriberBuilder::init` caps the
    /// `log` facade at, so it decides whether gpui's and wgpu's records are even
    /// constructed. `info` keeps the `warn` directives live without paying for
    /// wgpu's per-draw-call `trace!`s.
    #[test]
    fn the_default_filter_caps_the_log_facade_at_info() {
        let filter = EnvFilter::try_new(DEFAULT_FILTER).expect("filter parses");
        assert_eq!(
            filter.max_level_hint(),
            Some(tracing::level_filters::LevelFilter::INFO)
        );
    }
}
