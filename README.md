# telemetry-rs

The household's one Rust telemetry call. It writes no file, exports no metrics, reads no settings file of its own, and knows no broker, identity or token: the endpoint and the credential reach it as standard environment variables the fleet sets, or as a value the application hands it from its own Settings pane.

```toml
telemetry = { git = "https://github.com/radar-hooves/telemetry-rs", tag = "v0.3.0" }
```

## The one call

```rust
use std::sync::Mutex;
use tauri::{Manager, RunEvent};

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(Mutex::new(Some(telemetry::init(
                "bragi",
                env!("CARGO_PKG_VERSION"),
                &["bragi"],
            ))));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // Tauri drops NO managed state at exit, so this is the only thing
                // that flushes the batch processors. The Exit arm is on the main
                // thread, which is also where the Guard must be dropped.
                let guard = app
                    .state::<Mutex<Option<telemetry::Guard>>>()
                    .lock()
                    .expect("the telemetry guard")
                    .take();
                drop(guard);
            }
        });
}
```

Both halves are required. `app.manage(init(..))` on its own never drops the `Guard`, so whatever the batch processors hold at quit is lost. The state is a `Mutex<Option<Guard>>` because `Manager::unmanage` is deprecated and documented as unsafe — this is upstream's own advice.

Call `init` from the setup hook or `main`, never inside a Tokio task: it builds the exporters' blocking HTTP client. The same goes for the drop — after its bounded flush it joins that client's own runtime thread.

`telemetry::http_client()` returns an async `reqwest` client that carries W3C `traceparent` on every request and opens a client span recording the method, the host and the status — never a path, a query or an error string, any of which can carry a search term or a query-string credential. It sets a ten-second connect timeout, which a caller cannot add per request: only the total timeout has a `RequestBuilder` form, so without it a black-holed LAN address hangs for the whole total timeout instead of failing at connect. Add a per-request `timeout` where a call has its own deadline.

## The Settings pane

A Tauri app launched from the Dock inherits no fleet environment, so a desktop app takes the endpoint from its own Settings. The crate never reads that file; the app does, and hands the value over.

```rust
let exporter = telemetry::Exporter {
    endpoint: "https://otlp.example".to_owned(),
    headers_helper: Some("signet headers otlp".to_owned()),
};

match telemetry::probe(&exporter) {
    Ok(()) => guard.set_exporter(Some(exporter)),   // saves and repoints, live
    Err(why) => eprintln!("{why}"),                 // a class or a status, never a URL
}
```

`Guard::set_exporter` swaps the live OTLP log and span layers and flushes the previous ones on a plain thread; the stderr layer is untouched. It never fails and never panics — a helper that fails or an endpoint that will not build degrades to local only, exactly as `init` does. Both it and `probe` build the blocking client on a plain thread of their own, so an async Tauri command cannot make them panic; both still block the caller, so reach them through `spawn_blocking`.

**The environment wins.** Where the fleet set `OTEL_EXPORTER_OTLP_ENDPOINT`, `set_exporter` is a no-op that logs one line. `Guard::exporter()` gives the pane the value to show and `Guard::exporter_is_from_env()` tells it to show that value read-only. `Exporter::from_env()` is the crate's one reader of those variables, so `init` and the pane cannot disagree.

`probe` sends a single INFO record to `<endpoint>/v1/logs` through the same header client the exporters use, bounded by the OTLP timeout. `ProbeError` is `Helper`, `Connect`, `Tls`, `Timeout`, `Transport` or `Status(u16)` — a class or an HTTP status, and never a URL, a header value or a response body.

## The crash reporter

`init` also installs a panic hook, chained to whatever was there before (Rust's own default, or an app's own if it set one first, in which case that still runs too). A panic anywhere in the process becomes one ERROR event on `telemetry::panic`, force-allowed past the caller's allow-list exactly as the client span is, and flushed within a bounded budget before the chained hook runs.

The event always carries the thread name and the panic's file and line. The payload is carried only when its type proves it holds no runtime data: `panic!("a literal")`, `unreachable!()` and `todo!()` all downcast to `&'static str`, fixed at compile time, so that text is exported. `panic!("{}", value)`, `.expect(&built_string)` and anything else assembled at runtime downcasts to `String` instead; that shape is never inspected, and the event says the message was withheld.

`telemetry::report_error(context: &'static str)` gives an application the same force-allowed target for a caught error, without hand-rolling one: `context` must be `&'static str`, so only a compile-time literal can reach it.

```rust
match some_fallible_call() {
    Ok(value) => value,
    Err(_) => {
        telemetry::report_error("some_fallible_call failed");
        default_value()
    }
}
```

## The variables

| Variable | What it does |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The bearer-gated front door, and the only thing that makes the exporter read-only to a pane. Unset means local only until `set_exporter` says otherwise. Signal-specific `_LOGS_`/`_TRACES_` endpoints and `_TIMEOUT` are read by the SDK as usual. |
| `OTEL_EXPORTER_OTLP_HEADERS_HELPER` | A command printing a JSON object of header name to header value — `signet headers …` prints exactly this. Run once at init under `sh -c`, bounded to 10 s. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Static headers, in the standard `k=v,k=v` form. The helper's headers win where both set the same name. |
| `RUST_LOG` | The stderr layer only, defaulting to `info`. The developer's view is never allow-listed, though the exporter's own reporting is floored at INFO and capped at one line a minute. |

Batch sizing (`OTEL_BLRP_*`, `OTEL_BSP_*`) and protocol selection are left entirely to the SDK's own defaults and env handling; the crate overrides none of them.

## Local only

Endpoint unset and no pane has set one, or a helper that is missing, fails, times out or prints something that is not a JSON object: the crate installs the stderr layer alone, says which condition in one line, and returns a `Guard` anyway. It never errors and never panics, so a stranger's machine running the app behaves exactly this way. A second `init` builds nothing and returns an empty `Guard`.

## The allow-list

An event or span leaves the device only when its `target` equals an allow-list entry or sits beneath one, at INFO or more severe. Everything else stays on stderr. The list lives in the code beside the call, not in a file a user can widen by accident.

```rust
telemetry::init("thoth", env!("CARGO_PKG_VERSION"), &["thoth::playback"]);
tracing::info!(target: "thoth::dictation", "stays on this machine");
```

## Gates

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` are enforced in CI, one layer only; pre-commit's `cargo fmt` is a formatter, not a gate.

Contract: `rules-library/platform/telemetry.md`. Wiring: `docs/master/reference/guide-telemetry.md` §Rust.
