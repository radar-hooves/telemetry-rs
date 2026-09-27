//! The one predicate that decides what leaves the device.
//!
//! An event or span reaches the OTLP layers only when its `target` is on the
//! application's allow-list. That is what keeps listening history and dictation
//! local, and what stops the exporter's own internal logs re-exporting in a loop.

use tracing::Metadata;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::{FilterFn, filter_fn};

/// True when `target` is exactly `entry`, or a module beneath it.
fn matches(target: &str, entry: &str) -> bool {
    target == entry
        || target
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// True when `target` is on the allow-list. This crate's own client spans carry
/// only a method, a host and a status (`client.rs`), and its crash and error
/// events carry only a location, a thread and a content-safe payload
/// (`panic.rs`), so both are always allowed — under the same equality-or-`::`
/// rule as a caller's entry, never a bare prefix.
pub(crate) fn allowed(target: &str, allow: &[&str]) -> bool {
    matches(target, crate::client::TARGET)
        || matches(target, crate::panic::TARGET)
        || allow.iter().any(|entry| matches(target, entry))
}

/// The filter both OTLP layers share: the allow-list, with a level floor of INFO.
pub(crate) fn otlp_filter(
    allow: &'static [&'static str],
) -> FilterFn<impl Fn(&Metadata<'_>) -> bool> {
    filter_fn(move |meta: &Metadata<'_>| {
        *meta.level() <= tracing::Level::INFO && allowed(meta.target(), allow)
    })
    .with_max_level_hint(LevelFilter::INFO)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOW: &[&str] = &["bragi", "thoth::playback"];

    #[test]
    fn exact_match_is_allowed() {
        assert!(allowed("bragi", ALLOW));
        assert!(allowed("thoth::playback", ALLOW));
    }

    #[test]
    fn module_beneath_an_entry_is_allowed() {
        assert!(allowed("bragi::library::scan", ALLOW));
        assert!(allowed("thoth::playback::queue", ALLOW));
    }

    #[test]
    fn anything_else_is_denied() {
        assert!(!allowed("thoth", ALLOW));
        assert!(!allowed("thoth::dictation", ALLOW));
        assert!(!allowed("bragi_private", ALLOW), "a prefix is not a module");
        assert!(!allowed("opentelemetry_sdk", ALLOW), "no export loop");
        assert!(!allowed("", ALLOW));
    }

    #[test]
    fn the_crates_own_client_spans_are_always_allowed() {
        assert!(allowed(crate::client::TARGET, &[]));
        assert!(allowed("telemetry::client::retry", &[]));
        assert!(
            !allowed("telemetry::client_of_someone_else", &[]),
            "the force-allow is not a bare prefix either"
        );
    }

    #[test]
    fn the_crates_own_crash_and_error_events_are_always_allowed() {
        assert!(allowed(crate::panic::TARGET, &[]));
        assert!(
            !allowed("telemetry::panicky", &[]),
            "the force-allow is not a bare prefix either"
        );
    }
}
