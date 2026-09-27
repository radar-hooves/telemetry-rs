# telemetry

The household's one Rust telemetry call. Every Rust process — Tauri app, daemon, CLI — gets its `tracing` subscriber from here and owns nothing else about logging.

## Scope

- Does: install a stderr `fmt` layer for the developer, and, when an exporter is configured, an allow-listed OTLP/HTTP protobuf log and span exporter carrying `service.name`/`service.version`; run the header helper for the bearer; set the W3C propagator; hand out a `reqwest` client that injects `traceparent`; swap the live exporter at runtime and prove an endpoint answers; chain a panic hook that reports a crash and gives an application a content-free `report_error` for its own caught errors.
- The exporter arrives from the environment (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS_HELPER`) or from the application's own settings through `Guard::set_exporter` — an app launched from a Dock inherits no fleet environment. The environment comes first: where it set one, `set_exporter` is a no-op and `exporter_is_from_env` tells a pane to show it read-only.
- Does not: write a file, rotate, keep a log directory, push to Loki's own API, export metrics, read a settings file of its own, or know any broker, identity or token. The application owns its settings file; this crate is handed a value.
- The public surface is `init(service_name, service_version, allow) -> Guard`, `Guard` (`set_exporter`, `exporter`, `exporter_is_from_env`), `Exporter` (`from_env`), `probe(&Exporter) -> Result<(), ProbeError>`, `ProbeError`, `http_client()`, and `report_error(context: &'static str)`. Nothing is added to it without the contract changing first.

## The lines that matter

- An event or span leaves the device only if its `target` is on the caller's allow-list. That list is what keeps listening history and dictation local; widen it only with the reason written down.
- A header value, the helper's stdout, and anything derived from them never reach a log line or an error.
- The client span `http_client` opens is force-allowed past the caller's list, so it carries only a method, a host and a status. Never a URL, path, query or error string: `reqwest`'s own error `Display` and `Debug` both carry the full URL, which is why the crate supplies its own span backend rather than `reqwest-tracing`'s default, and why `ProbeError` names a class or a status and is never built from an error's message.
- The allow-list filter sits outside the reload slot, not inside it. It never changes, and `reload::Handle::reload` cannot carry a `Filtered` layer: a layer swapped in that way is never handed a filter id.
- `reqwest`'s blocking client panics when it is built or used inside a Tokio runtime, and a Settings pane's save and test buttons are async commands. `set_exporter` and `probe` run that work on a plain thread of their own; only `init` still leaves the caller responsible, and says so.
- Anything missing, failing or malformed degrades to stderr-only with one `debug!` line. A stranger's machine running the app must behave exactly that way, and `init` must never return an error or panic.
- Batch sizing, protocol selection and export timeouts are the SDK's own env handling. The crate overrides none of them: hardcoding a value here takes the standard variable away from the fleet. `http_client`'s connect timeout is the exception, and only because reqwest has no per-request form of it.
- Tauri drops no managed state at exit, so the wiring is always both halves: manage a `Mutex<Option<Guard>>` and take it in the `RunEvent::Exit` arm. Documenting only the `manage` half ships a crate whose flush never runs.
- A crash event always carries the thread name and the panic's file and line. It carries the panic payload only when its type proves it cannot hold runtime data — a `&'static str` downcast, true only for a compile-time literal (`panic!("literal")`, `unreachable!()`, `todo!()`) — and otherwise says the message was withheld; `report_error`'s `context: &'static str` rests on the same rule at the type level. The panic hook chains to whatever was installed before it, and never recurses into a second flush attempt when the exporter's own `force_flush` is what panicked — a thread-local marks the crate's own flush worker so `report` can tell that case apart from an unrelated panic elsewhere.

Contract: `rules-library/platform/telemetry.md`. Wiring: `docs/master/reference/guide-telemetry.md` §Rust. Design: `radar-hooves/master-project#314` (07/09/2026), ledger `#330`.
