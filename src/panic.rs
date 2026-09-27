//! The one crash event, and the one content-free error event beside it.
//!
//! A panic anywhere in the process is reported as one ERROR event on this
//! crate's own force-allowed target (`allow::allowed` treats it exactly as it
//! treats `client::TARGET`), carrying the thread name and the panic location
//! always. The payload is carried only when its *type* proves it cannot hold
//! runtime data: `panic!("literal")`, `unreachable!()` and `todo!()` all
//! downcast to `&'static str`, a value fixed at compile time, while
//! `panic!("{}", x)`, `.expect(&format!(...))` and anything built with
//! interpolation downcast to `String` instead — and that shape is never
//! inspected, only named as withheld. This is the same rule `client.rs`
//! applies to a failed request: a class or a location is safe to export by
//! construction, a message is not, unless its type says otherwise.
//!
//! [`report_error`] gives an application the same content-free target for its
//! own caught errors, without hand-rolling one: `context` is typed
//! `&'static str`, so only a compile-time literal can be passed without
//! deliberately defeating `&'static` — the type is the safety property.

use std::cell::Cell;
use std::panic::PanicHookInfo;
use std::sync::Mutex;
use std::time::Duration;

use opentelemetry_sdk::logs::SdkLoggerProvider;

/// This crate's own force-allowed target — a crash or a caught error is never
/// a listening history or a dictation string, so it needs no allow-list entry
/// from the caller, exactly as `client::TARGET` needs none.
pub(crate) const TARGET: &str = "telemetry::panic";

/// How long the panic hook waits for its own flush. The same figure as
/// [`crate::SHUTDOWN_BUDGET`] — a separate constant because a panic hook that
/// waited longer would risk delaying whatever the process does next: an abort
/// happens the instant this hook returns, in a release profile built with
/// `panic = "abort"`, which is common in a Tauri app to shrink the binary.
const FLUSH_BUDGET: Duration = Duration::from_secs(2);

/// The live logs provider the hook flushes through, mirrored beside whichever
/// `Guard` currently owns the pipeline. The hook is process-global — installed
/// once, outliving any one `Guard::set_exporter` swap — so it cannot borrow a
/// single `Guard`'s own field; this is the one place both sides agree on.
static LIVE: Mutex<Option<SdkLoggerProvider>> = Mutex::new(None);

thread_local! {
    /// Set for the lifetime of the one thread [`flush`] spawns to call
    /// `force_flush`, and nowhere else. If the exporter panics from inside
    /// that call, the panic hook fires again — hooks are process-global, not
    /// per-thread — on this very thread, and this flag is how [`report`] tells
    /// that case apart from an unrelated panic on some other thread: it must
    /// not spawn a second worker from inside the first, which is exactly the
    /// unbounded recursion a broken exporter would otherwise trigger.
    static IS_FLUSH_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Point the panic hook at the currently live logs provider, or at none.
/// Called wherever `lib.rs` starts or replaces one: [`crate::init`] for the
/// first, [`crate::Guard::install`] for every swap after it, and `Guard`'s
/// `Drop` on the way out.
pub(crate) fn set_live(logs: Option<&SdkLoggerProvider>) {
    if let Ok(mut live) = LIVE.lock() {
        *live = logs.cloned();
    }
}

/// Install the crash reporter, chained to whatever hook is already in place —
/// Rust's own default, or an app's, when `init` runs after the app set one.
/// It never replaces that hook, only wraps it, so the default stderr panic
/// message (and `RUST_BACKTRACE`) still prints exactly as it would have, and
/// an app's own hook still runs too.
pub(crate) fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        previous(info);
    }));
}

/// Emit the crash as one ERROR event and flush it, bounded, before returning
/// control to the chained hook.
///
/// Never panics, never blocks longer than [`FLUSH_BUDGET`], and never
/// recurses: a panic on [`flush`]'s own worker thread — the exporter itself
/// breaking — hits this same hook again, since a hook is process-global, and
/// [`IS_FLUSH_WORKER`] is what stops it spawning a second worker from inside
/// the first. That thread's default panic output still prints, through
/// whatever hook this one is chained to; it only skips another report.
fn report(info: &PanicHookInfo<'_>) {
    if IS_FLUSH_WORKER.with(Cell::get) {
        return;
    }
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let (file, line) = info
        .location()
        .map(|l| (l.file(), l.line()))
        .unwrap_or(("<unknown>", 0));

    match literal_payload(info.payload()) {
        Some(payload) => tracing::error!(target: TARGET, thread, file, line, payload, "panic"),
        None => {
            tracing::error!(target: TARGET, thread, file, line, "panic, message withheld: not a compile-time literal")
        }
    }
    flush();
}

/// A content-free way to record that something went wrong, for an application
/// that would otherwise hand-roll its own force-allowed target for a caught
/// error. `context` is a compile-time literal by construction: `&'static str`
/// cannot carry a value built at runtime without deliberately leaking one, the
/// same trust boundary `init`'s own `allow: &'static [&'static str]` already
/// rests on.
pub fn report_error(context: &'static str) {
    tracing::error!(target: TARGET, context, "error");
}

/// `panic!("literal")`, `unreachable!()` and `todo!()` all downcast to
/// `&'static str` — a payload fixed at compile time. `panic!("{}", x)`,
/// `.expect(&built_string)` and anything else assembled at runtime downcasts
/// to `String` instead, and is never inspected: that shape is exactly the one
/// that can hold what the panic was actually about.
fn literal_payload(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload.downcast_ref::<&str>().copied()
}

/// Force-flush the live logs provider on a thread of its own, bounded by
/// [`FLUSH_BUDGET`]. A thread of its own, rather than a call right here,
/// because `force_flush` can itself hang or panic if the exporter is what
/// broke; either way this function returns at the deadline instead of waiting
/// on it, and a panic on that other thread only ever kills that thread — after
/// marking it with [`IS_FLUSH_WORKER`] first, so [`report`] recognises that
/// panic as its own worker rather than spawning another one.
///
/// A `try_lock` stands in for a normal blocking one for the same reason: a
/// panic raised while some other caller is already inside `set_live`, on the
/// same thread that is now trying to read [`LIVE`], would otherwise deadlock
/// this hook against itself. `try_lock` cannot block, so that case degrades to
/// "this crash was not flushed" instead.
fn flush() {
    let Ok(guard) = LIVE.try_lock() else { return };
    let Some(logs) = guard.clone() else { return };
    drop(guard);

    let (tx, rx) = std::sync::mpsc::channel();
    if std::thread::Builder::new()
        .spawn(move || {
            IS_FLUSH_WORKER.with(|flag| flag.set(true));
            let _ = logs.force_flush();
            let _ = tx.send(());
        })
        .is_ok()
    {
        let _ = rx.recv_timeout(FLUSH_BUDGET);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Providers, otlp_layers, subscriber};
    use opentelemetry::InstrumentationScope;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::{
        InMemoryLogExporter, LogProcessor, SdkLogRecord, SimpleLogProcessor,
    };
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};
    use serial_test::serial;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    fn in_memory() -> (Providers, InMemoryLogExporter) {
        let logs = InMemoryLogExporter::default();
        let spans = InMemorySpanExporter::default();
        let providers = Providers {
            logs: SdkLoggerProvider::builder()
                .with_log_processor(SimpleLogProcessor::new(logs.clone()))
                .build(),
            traces: opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_span_processor(SimpleSpanProcessor::new(spans))
                .build(),
        };
        (providers, logs)
    }

    /// Run `f` (which must panic) under a hook that calls only [`report`],
    /// never chaining anywhere, and restore whatever hook was live before —
    /// so these tests never leave the process's panic hook in a state another
    /// test would inherit.
    fn under_bare_report_hook(f: impl FnOnce()) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(report));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::panic::set_hook(previous);
    }

    #[test]
    #[serial]
    fn a_literal_payload_panic_exports_its_message() {
        let (providers, logs) = in_memory();
        set_live(Some(&providers.logs));
        let (subscriber, _reload) = subscriber(&[], otlp_layers("test-service", &providers));

        tracing::subscriber::with_default(subscriber, || {
            under_bare_report_hook(|| panic!("a literal panic message"));
        });
        set_live(None);

        let exported = logs.get_emitted_logs().expect("logs");
        let rendered = format!("{exported:?}");
        assert!(rendered.contains("a literal panic message"), "{rendered}");
        assert!(rendered.contains("panic.rs"), "{rendered}");
    }

    #[test]
    #[serial]
    fn a_formatted_payload_panic_exports_no_message_text() {
        let (providers, logs) = in_memory();
        set_live(Some(&providers.logs));
        let (subscriber, _reload) = subscriber(&[], otlp_layers("test-service", &providers));

        let runtime_value = "a-runtime-value-nobody-should-see".to_owned();
        tracing::subscriber::with_default(subscriber, || {
            under_bare_report_hook(move || panic!("{runtime_value}"));
        });
        set_live(None);

        let exported = logs.get_emitted_logs().expect("logs");
        let rendered = format!("{exported:?}");
        assert!(!rendered.contains("a-runtime-value"), "{rendered}");
        assert!(rendered.contains("withheld"), "{rendered}");
    }

    #[test]
    #[serial]
    fn the_hook_chains_to_whatever_was_there_before() {
        set_live(None);
        let ran_previous = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&ran_previous);
        let outer_previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            marker.store(true, Ordering::SeqCst);
        }));

        install();
        let _ = std::panic::catch_unwind(|| panic!("chained"));

        std::panic::set_hook(outer_previous);
        assert!(
            ran_previous.load(Ordering::SeqCst),
            "the hook that was live before install() must still run"
        );
    }

    /// A processor whose own `force_flush` panics, standing in for an exporter
    /// that breaks while exporting the crash it is meant to carry, and counts
    /// how many times it was actually called — the number that proves whether
    /// the hook recursed into a second flush attempt or not.
    #[derive(Debug, Clone)]
    struct PanicsOnFlush {
        calls: Arc<AtomicUsize>,
    }

    impl LogProcessor for PanicsOnFlush {
        fn emit(&self, _record: &mut SdkLogRecord, _scope: &InstrumentationScope) {}

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("the exporter itself panicked");
        }
    }

    #[test]
    #[serial]
    fn a_panic_inside_export_does_not_hang_or_recurse() {
        let calls = Arc::new(AtomicUsize::new(0));
        let logs = SdkLoggerProvider::builder()
            .with_resource(Resource::builder_empty().build())
            .with_log_processor(PanicsOnFlush {
                calls: Arc::clone(&calls),
            })
            .build();
        set_live(Some(&logs));

        let started = Instant::now();
        under_bare_report_hook(|| panic!("a literal panic message"));
        set_live(None);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a panicking exporter must not hang the reporting hook: took {:?}",
            started.elapsed()
        );
        // A worker thread that spawned another on its own panic would keep
        // doing so well past this test's own bounded wait; give that failure
        // mode a moment to show itself before trusting the count.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the exporter's own panic must not re-trigger the reporting hook"
        );
    }
}
