//! The span this crate's own outbound HTTP client opens.
//!
//! `reqwest-tracing`'s `DefaultSpanBackend` records `error.message` and
//! `error.cause_chain` on a failed request, and both are built from a
//! `reqwest::Error` whose `Display` appends `" for url ({url})"` and whose
//! `Debug` carries a `url` field — the full URL, path and query. Because this
//! target is force-allowed past the caller's allow-list, that would export a
//! search term or a query-string credential off the device with no allow-list
//! control. So the household backend records the method, the host, the path
//! and the status — the span's own start and close timestamps are its
//! duration, exactly as `command_span` documents for its own spans — and
//! never the query string or an error string.
//!
//! The path was added on the operator's ruling of 28/09/2026 (see the crate
//! CHANGELOG): `/Items/{id}` or `/rest/search3.view` is his own listening
//! history and now travels to his stack. The query string stays withheld —
//! that is where a Subsonic request signs its token — and `url::Url::path()`
//! never includes it, so the split is structural, not a redaction pass.

use reqwest::Request;
use reqwest_middleware::{Error, Result};
use reqwest_tracing::ReqwestOtelSpanBackend;
use tracing::{Span, field::Empty, info_span};

/// The `target` of the span below. Force-allowed by `allow::allowed`, so it is
/// this crate's own constant rather than a claim about another crate's layout.
pub(crate) const TARGET: &str = "telemetry::client";

pub(crate) struct HouseholdSpanBackend;

impl ReqwestOtelSpanBackend for HouseholdSpanBackend {
    fn on_request_start(request: &Request, _: &mut http::Extensions) -> Span {
        let url = request.url();
        info_span!(
            target: TARGET,
            "HTTP request",
            otel.kind = "client",
            otel.name = %request.method(),
            otel.status_code = Empty,
            http.request.method = %request.method(),
            server.address = %url.host_str().unwrap_or_default(),
            server.port = url.port_or_known_default().unwrap_or_default() as i64,
            url.path = %url.path(),
            http.response.status_code = Empty,
        )
    }

    fn on_request_end(span: &Span, outcome: &Result<reqwest::Response>, _: &mut http::Extensions) {
        match outcome {
            Ok(response) => {
                span.record("http.response.status_code", response.status().as_u16());
                if response.status().is_server_error() {
                    span.record("otel.status_code", "ERROR");
                }
            }
            Err(error) => {
                span.record("otel.status_code", "ERROR");
                // Deliberately nothing from the error itself: see the module doc.
                if let Error::Reqwest(error) = error
                    && let Some(status) = error.status()
                {
                    span.record("http.response.status_code", status.as_u16());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry;

    use super::*;

    #[derive(Default)]
    struct Captured(Arc<Mutex<Option<String>>>);

    struct Recorder(String);

    impl Visit for Recorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        }
    }

    impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S>
        for Captured
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _: Context<'_, S>) {
            let mut recorder = Recorder(String::new());
            attrs.record(&mut recorder);
            *self.0.lock().unwrap() = Some(recorder.0);
        }
    }

    /// Proves the path/query split at its source, with no network involved:
    /// `on_request_start` runs before a request is ever sent.
    #[test]
    fn on_request_start_records_the_path_but_never_the_query() {
        let captured = Captured::default();
        let fields = captured.0.clone();
        let dispatch = tracing::Dispatch::new(registry().with(captured));
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let request = reqwest::Client::new()
            .get("https://music.example.com/rest/search3.view?u=demo&t=leaked&s=salt")
            .build()
            .expect("a plain GET builds");
        let _span = HouseholdSpanBackend::on_request_start(&request, &mut http::Extensions::new());

        let fields = fields.lock().unwrap().clone().expect("a span was opened");
        assert!(fields.contains("/rest/search3.view"), "{fields}");
        assert!(!fields.contains("leaked"), "{fields}");
        assert!(!fields.contains("salt"), "{fields}");
        assert!(!fields.contains("demo"), "{fields}");
    }
}
