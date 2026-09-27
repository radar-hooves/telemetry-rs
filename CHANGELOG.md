# Changelog

## 0.4.0

**Breaking:** a panic event no longer withholds a formatted message. v0.3.0's rule — export the payload only when its type proves it holds no runtime data, and otherwise say the message was withheld — is overruled by the operator (28/09/2026): "the telemetry stack is my own and I need to have high quality telemetry in order to be able to develop and improve these systems, so we shouldn't be timid about what we capture if it's going to lead to a better dev experience." The OTLP endpoint is his own self-hosted collector, so nothing this crate now captures leaves his estate.

A crash event carries the panic's full message — `panic!("literal")`, `unreachable!()`, `todo!()`, `panic!("{}", x)` and `.expect(&built)` alike — plus a backtrace, captured with `Backtrace::force_capture()` so it is there whether or not `RUST_BACKTRACE` is set. The thread name, file, line, service name and version are carried exactly as in v0.3.0. The chained hook, the bounded 2 s flush and the recursion guard are unchanged.

Added to the public surface — `report_error_with_cause(context: &'static str, error: &dyn std::error::Error)`, `report_error`'s companion: it carries the error's own message and its whole `source()` chain, so a caller stops flattening a real error to a static label. `report_error(context: &'static str)` is unchanged, for a caller with no `Error` value to hand.

## 0.3.0

`init` now installs a panic hook: a crash anywhere in the process becomes one ERROR event on the crate's own force-allowed `telemetry::panic` target, carrying the thread name and the panic's file and line always. The payload is exported only when its type proves it cannot hold runtime data — a `panic!("literal")`, `unreachable!()` or `todo!()` payload downcasts to `&'static str`, fixed at compile time — and otherwise the event says the message was withheld, never inspecting a `String` payload built at runtime. The hook chains to whatever was installed before it (Rust's own default, or an app's own), flushes the live logs provider on a thread of its own bounded to the same budget the `Guard` uses for shutdown, and never recurses into a second flush attempt when the exporter's own `force_flush` is what panicked. Added to the public surface — `report_error(context: &'static str)`, a content-free way for an application to record a caught error on the same force-allowed target without hand-rolling one.

## 0.2.0

The exporter may now arrive from the application's own settings, not only from the fleet's environment: a Tauri app launched from the Dock inherits no environment at all. Added to the public surface — `Exporter` and `Exporter::from_env`, `Guard::set_exporter`, `Guard::exporter`, `Guard::exporter_is_from_env`, and `probe` with `ProbeError`. `set_exporter` swaps the live OTLP log and span layers through a `reload::Layer` and retires the previous providers on a plain thread; the stderr layer never reloads. Where the environment set the exporter it still wins, and `set_exporter` is a no-op. `set_exporter` and `probe` run the blocking `reqwest` work on a plain thread of their own, so an async Tauri command cannot make them panic. `probe` sends one OTLP/HTTP protobuf log record to `<endpoint>/v1/logs` through the same header client the exporters use and reports a transport class or an HTTP status — never a URL, a header value or a response body.
