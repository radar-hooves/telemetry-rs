//! The household's one Rust telemetry call.
//!
//! [`init`] installs a stderr layer for the developer and, when an exporter is
//! configured, an allow-listed OTLP/HTTP protobuf log and span exporter for the
//! corpus; [`http_client`] carries W3C trace context outbound; a panic anywhere
//! in the process reaches the corpus too, through the same one call — see
//! `panic` for the content-safety rule that governs what a crash may carry.
//! [`sample_process_metrics`] is the same idea for the process's own RSS and
//! CPU: spawn it once on the caller's runtime and it reports every 60s,
//! forever, on the same force-allowed footing as a client span or a crash.
//!
//! The exporter may arrive from the environment, as `OTEL_EXPORTER_OTLP_*`, or
//! from the application's own settings through [`Guard::set_exporter`] — an app
//! launched from a Dock inherits no fleet environment. The environment comes
//! first: where it set one, a pane cannot change it. [`probe`] proves an endpoint
//! answers before an application saves it.
//!
//! The crate writes no file, reads no settings file of its own, and knows no
//! broker, identity or token.
//!
//! Contract: `rules-library/platform/telemetry.md`; wiring:
//! `docs/master/reference/guide-telemetry.md` §Rust.

mod allow;
mod bearer;
mod client;
mod panic;
mod probe;
mod process;

pub use panic::{report_error, report_error_with_cause};
pub use probe::{ProbeError, probe};
pub use process::sample_process_metrics;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator};
use opentelemetry_semantic_conventions::resource::SERVICE_VERSION;
use serde::{Deserialize, Serialize};
use tracing::Metadata;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::{EnvFilter, FilterExt, dynamic_filter_fn};
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry, fmt, reload};

/// The total a shutdown may spend flushing, so neither exit nor a settings change
/// is delayed.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// A household outbound call that cannot connect in this long is dead. It has to
/// be set on the client: only the total timeout has a per-request form
/// (`RequestBuilder::timeout`), so a caller cannot supply this one itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the header helper may take, wherever it is run.
const HELPER_BUDGET: Duration = Duration::from_secs(10);

/// Claimed by the first [`init`]; a later call builds nothing.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// The two OTLP layers, swapped as a unit by [`Guard::set_exporter`]; empty is
/// local only. The allow-list filter sits *outside* this slot, because it never
/// changes and `reload::Handle::reload` cannot carry a `Filtered` layer — the
/// swapped-in layer would never be handed a filter id.
type OtlpLayers = Vec<Box<dyn Layer<Registry> + Send + Sync>>;

/// The end of the reload slot [`Guard`] keeps, so a later exporter can replace
/// the live layers without touching the stderr layer beside them.
type OtlpHandle = reload::Handle<OtlpLayers, Registry>;

/// The two providers, kept together because they are built and shut down together.
struct Providers {
    logs: SdkLoggerProvider,
    traces: SdkTracerProvider,
}

/// Where telemetry goes and how it authenticates: the same two facts
/// `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS_HELPER` carry.
/// It is serde-shaped so an application can keep it in its own settings — this
/// crate still reads and writes no settings file of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exporter {
    /// The collector's base address, as `OTEL_EXPORTER_OTLP_ENDPOINT` means it:
    /// the signal path (`/v1/logs`) is appended to it, not replaced.
    pub endpoint: String,
    /// A command printing a JSON object of header name to header value, run once
    /// under `sh -c` whenever this exporter is installed or probed.
    pub headers_helper: Option<String>,
}

impl Exporter {
    /// The fleet's answer, read from the standard variables — the crate's one
    /// reader of them, so [`init`] and a Settings pane cannot disagree about what
    /// the environment says. `None` when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?,
            headers_helper: std::env::var("OTEL_EXPORTER_OTLP_HEADERS_HELPER").ok(),
        })
    }
}

/// What [`init`] installed, and everything a later swap needs to rebuild.
struct Installed {
    reload: OtlpHandle,
    service_name: &'static str,
    service_version: &'static str,
}

/// Keeps the export pipeline alive. Hold it for the process lifetime, and drop it
/// at exit: dropping is what flushes the batch processors and shuts the providers
/// down, within [`SHUTDOWN_BUDGET`].
///
/// **Tauri does not drop managed state at exit.** `app.manage(init(..))` on its
/// own means this `Drop` never runs and whatever the batch processors hold at
/// quit is lost. Manage a `Mutex<Option<Guard>>` and take it in the
/// `RunEvent::Exit` arm — see [`init`] for the snippet.
///
/// Drop it from a plain thread, never from a Tokio worker: after that bounded
/// flush the exporters' blocking `reqwest` client is dropped, and its own `Drop`
/// joins the `reqwest-internal-sync-runtime` thread with no timeout of its own.
/// It returns as soon as the channel closes, but it is a blocking join, so a
/// Tokio worker would be parked for its duration. Tauri's `Exit` arm runs on the
/// main thread, which satisfies that.
///
/// [`Guard::set_exporter`] repoints the live pipeline at runtime, so the same
/// `Guard` is also what a Settings pane holds.
#[derive(Default)]
pub struct Guard {
    providers: Mutex<Option<Providers>>,
    retiring: Mutex<Option<std::thread::JoinHandle<()>>>,
    installed: Option<Installed>,
    exporter: Mutex<Option<Exporter>>,
    from_env: bool,
}

impl Guard {
    /// Point the live log and span export at `exporter`, or at nothing. The stderr
    /// layer is untouched, and events already in flight are not lost: the layers
    /// are swapped first, then the previous providers are flushed and shut down on
    /// a plain thread within [`SHUTDOWN_BUDGET`], off the caller's.
    ///
    /// A no-op when the environment set the exporter — the fleet's variables win
    /// over a pane — and a no-op on a [`Guard`] that installed nothing. Never
    /// fails and never panics: a helper that fails or an endpoint that will not
    /// build degrades to local only with one `debug!` line, exactly as [`init`]
    /// does.
    ///
    /// Safe to call from anywhere: the exporters' blocking `reqwest` client is
    /// built on a plain thread of its own, so the caller's context cannot make it
    /// panic. It still blocks the caller while the header helper runs, so from an
    /// async command reach it through `spawn_blocking`.
    pub fn set_exporter(&self, exporter: Option<Exporter>) {
        if self.from_env {
            tracing::debug!(
                "telemetry: the environment set the exporter and wins; set_exporter installed nothing"
            );
            return;
        }
        let Some(installed) = &self.installed else {
            tracing::debug!(
                "telemetry: this Guard installed no layers; set_exporter changed nothing"
            );
            return;
        };
        let built = match &exporter {
            None => None,
            Some(wanted) => match headers_for(wanted) {
                Ok(headers) => build_off_thread(installed, &headers, &wanted.endpoint),
                Err(why) => {
                    tracing::debug!("telemetry is local only: {why}");
                    None
                }
            },
        };
        if exporter.is_some() && built.is_none() {
            tracing::debug!("telemetry is local only: no exporter could be built");
        }
        self.install(built, exporter);
    }

    /// The exporter this process is configured for, for a pane to show. A
    /// configured exporter whose helper failed or whose endpoint would not build
    /// still reads back here; the pipeline degraded to local only and said so.
    pub fn exporter(&self) -> Option<Exporter> {
        lock(&self.exporter).clone()
    }

    /// True when the environment set the exporter, which makes it read-only:
    /// [`Guard::set_exporter`] will not change it, so a pane should say so.
    pub fn exporter_is_from_env(&self) -> bool {
        self.from_env
    }

    /// Swap the live layers, then retire what they replaced. Reload first, so
    /// nothing emitted after this call reaches the old destination.
    ///
    /// The reload and the record of what is live are one critical section. Two
    /// callers that interleave them — two saves from a pane, a retry racing the
    /// first — would otherwise leave the subscriber pointed at providers the
    /// other caller has just shut down, silently and with nothing to see it.
    fn install(&self, providers: Option<Providers>, exporter: Option<Exporter>) {
        let Some(installed) = &self.installed else {
            return;
        };
        let layers = providers
            .as_ref()
            .map(|p| otlp_layers(installed.service_name, p))
            .unwrap_or_default();
        let mut live = lock(&self.providers);
        if installed.reload.reload(layers).is_err() {
            tracing::debug!("telemetry: the layer stack is gone; the exporter is unchanged");
            drop(live);
            self.retire(providers);
            return;
        }
        let previous = std::mem::replace(&mut *live, providers);
        panic::set_live(live.as_ref().map(|p| &p.logs));
        *lock(&self.exporter) = exporter;
        drop(live);
        self.retire(previous);
    }

    /// Shut down providers nothing is wired to any more, off the caller's thread:
    /// the flush may take the whole budget and a swap can come from a Tauri
    /// command. The handle is kept so a quit moments after a swap still waits for
    /// that flush; replacing it detaches a flush whose own budget has passed.
    fn retire(&self, providers: Option<Providers>) {
        let Some(providers) = providers else {
            return;
        };
        *lock(&self.retiring) = Some(std::thread::spawn(move || shutdown(providers)));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // An empty `Guard` from a second `init()` call installed nothing, so it
        // must not clear the one real `Guard`'s record of the live provider.
        if self.installed.is_some() {
            panic::set_live(None);
        }
        if let Some(providers) = lock(&self.providers).take() {
            shutdown(providers);
        }
        // A swap moments before the quit left a flush in flight. It carries its
        // own budget, so waiting for it costs at most one more.
        if let Some(retiring) = lock(&self.retiring).take() {
            let _ = retiring.join();
        }
    }
}

/// A poisoned lock is no reason to skip a flush or a swap: the data behind it is
/// an `Option` either way.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Flush and shut down both providers, bounded by [`SHUTDOWN_BUDGET`].
fn shutdown(providers: Providers) {
    let deadline = Instant::now() + SHUTDOWN_BUDGET;
    let left = || deadline.saturating_duration_since(Instant::now());
    let _ = providers.logs.shutdown_with_timeout(left());
    let _ = providers.traces.shutdown_with_timeout(left());
}

/// Install the process's telemetry. Call once, at the entry point, before
/// anything logs, and keep the returned [`Guard`] for the process lifetime.
///
/// `allow` is the list of `tracing` targets whose events and spans may leave the
/// device: a target matches when it equals an entry or sits beneath one
/// (`bragi` allows `bragi::library`). Everything else stays on stderr. That list
/// is the application's own decision and belongs in its code, not in a file a
/// user can widen by accident.
///
/// The exporter comes from the environment, through [`Exporter::from_env`]. An
/// app launched from a Dock has none, so [`Guard::set_exporter`] can supply one
/// at runtime from the application's own settings — but only where the
/// environment set nothing.
///
/// A panic anywhere in the process afterwards is reported as one crash event,
/// content-safe by construction (see `panic`'s module docs), and flushed within
/// a bounded budget before the chained, previously installed hook — Rust's own
/// default, or an app's own if it set one first — runs in turn.
///
/// Never fails: an unset endpoint, or a missing or failing header helper, degrades
/// to stderr only with one line saying which. The only blocking work is the header
/// helper, bounded to ten seconds. A second call builds nothing and returns an
/// empty [`Guard`].
///
/// Call it from a plain thread — a Tauri app's `setup` hook on the main thread,
/// or `main` — and never from inside a Tokio task: it constructs the exporters'
/// blocking `reqwest` client, which panics if built in an async context.
///
/// Tauri does not drop managed state at exit, so the [`Guard`] must be taken back
/// out and dropped in the `RunEvent::Exit` arm or nothing flushes. `Manager::
/// unmanage` is deprecated and documented as unsafe, so the state is a
/// `Mutex<Option<Guard>>` and the exit arm takes it — upstream's own advice:
///
/// ```ignore
/// use std::sync::Mutex;
/// use tauri::{Manager, RunEvent};
///
/// tauri::Builder::default()
///     .setup(|app| {
///         app.manage(Mutex::new(Some(telemetry::init(
///             "bragi",
///             env!("CARGO_PKG_VERSION"),
///             &["bragi"],
///         ))));
///         Ok(())
///     })
///     .build(tauri::generate_context!())
///     .expect("build")
///     .run(|app, event| {
///         if let RunEvent::Exit = event {
///             // Tauri drops no managed state at exit; this is what flushes.
///             // The Exit arm is on the main thread, off any async runtime.
///             let guard = app
///                 .state::<Mutex<Option<telemetry::Guard>>>()
///                 .lock()
///                 .expect("the telemetry guard")
///                 .take();
///             drop(guard);
///         }
///     });
/// ```
pub fn init(
    service_name: &'static str,
    service_version: &'static str,
    allow: &'static [&'static str],
) -> Guard {
    if INITIALISED.swap(true, Ordering::SeqCst) {
        tracing::debug!("telemetry: init was already called; this call installed nothing");
        return Guard::default();
    }
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let from_env = Exporter::from_env();
    let plan = plan(from_env.as_ref());
    let providers = match &plan {
        Plan::LocalOnly { .. } => None,
        Plan::Otlp(headers) => build_providers(service_name, service_version, headers, None),
    };
    let layers = providers
        .as_ref()
        .map(|p| otlp_layers(service_name, p))
        .unwrap_or_default();
    let (subscriber, reload) = subscriber(allow, layers);
    let installed = subscriber.try_init().is_ok();

    if !installed {
        // Something else owns the global subscriber, so our layers reach nothing.
        // Shut the providers down rather than hand back a live-looking Guard over
        // orphaned exporter threads.
        if let Some(providers) = providers {
            shutdown(providers);
        }
        tracing::debug!("telemetry: another subscriber is installed; this call installed nothing");
        return Guard::default();
    }
    match plan {
        Plan::LocalOnly { why } => tracing::debug!("telemetry is local only: {why}"),
        Plan::Otlp(_) if providers.is_none() => tracing::debug!("telemetry: no exporter built"),
        Plan::Otlp(_) => {}
    }
    panic::set_live(providers.as_ref().map(|p| &p.logs));
    panic::install();
    Guard {
        providers: Mutex::new(providers),
        retiring: Mutex::new(None),
        from_env: from_env.is_some(),
        exporter: Mutex::new(from_env),
        installed: Some(Installed {
            reload,
            service_name,
            service_version,
        }),
    }
}

/// An async `reqwest` client whose every request opens a client span and carries
/// W3C `traceparent`. Use it for outbound HTTP instead of building your own.
///
/// The span records the method, the host and the status only — never a path, a
/// query or an error string, all of which can carry private material.
///
/// The client carries a ten-second connect timeout, because a caller cannot add
/// one per request: only the total timeout has a `RequestBuilder` form, so a
/// black-holed address would otherwise hang for the whole total timeout instead
/// of failing at connect. Set a per-request `timeout` on top where the call has
/// its own deadline.
pub fn http_client() -> reqwest_middleware::ClientWithMiddleware {
    ensure_crypto_provider();
    let inner = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("a reqwest client with no TLS or resolver configuration of its own");
    reqwest_middleware::ClientBuilder::new(inner)
        .with(reqwest_tracing::TracingMiddleware::<
            client::HouseholdSpanBackend,
        >::new())
        .build()
}

/// What the environment asks for.
enum Plan {
    LocalOnly { why: &'static str },
    Otlp(HashMap<String, String>),
}

fn plan(exporter: Option<&Exporter>) -> Plan {
    let Some(exporter) = exporter else {
        return Plan::LocalOnly {
            why: "OTEL_EXPORTER_OTLP_ENDPOINT is unset",
        };
    };
    match headers_for(exporter) {
        Ok(headers) => Plan::Otlp(headers),
        Err(why) => Plan::LocalOnly { why },
    }
}

/// The headers an exporter authenticates with: the helper's output, or none where
/// it names no helper. Shared by [`init`], [`Guard::set_exporter`] and [`probe`],
/// so a pane's endpoint is credentialed exactly as the fleet's is.
fn headers_for(exporter: &Exporter) -> Result<HashMap<String, String>, &'static str> {
    match &exporter.headers_helper {
        None => Ok(HashMap::new()),
        Some(command) => bearer::run_helper(command, HELPER_BUDGET),
    }
}

/// The join the SDK makes from `OTEL_EXPORTER_OTLP_ENDPOINT`, made here too, so a
/// pane's endpoint means exactly what the variable means. One trailing slash, as
/// upstream's `build_endpoint_uri` does it — never every trailing slash, which
/// would rewrite an endpoint the variable would have kept.
fn signal_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.strip_suffix('/').unwrap_or(base))
}

/// The layer stack, built once and installed either globally by [`init`] or
/// locally by a test through `tracing::subscriber::with_default`. Only the OTLP
/// half reloads; the developer's stderr view is fixed for the process.
fn subscriber(
    allow: &'static [&'static str],
    layers: OtlpLayers,
) -> (impl tracing::Subscriber + Send + Sync + 'static, OtlpHandle) {
    let (otlp, reload) = reload::Layer::new(layers);
    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(env_filter().and(exporter_noise_filter()));
    let subscriber = tracing_subscriber::registry()
        .with(otlp.with_filter(allow::otlp_filter(allow)))
        .with(stderr);
    (subscriber, reload)
}

/// The pair of layers that carry allowed events and spans to `providers`. Neither
/// carries a filter of its own: the allow-list lives outside the reload slot.
fn otlp_layers(service_name: &'static str, providers: &Providers) -> OtlpLayers {
    vec![
        Box::new(OpenTelemetryTracingBridge::new(&providers.logs)),
        Box::new(tracing_opentelemetry::layer().with_tracer(providers.traces.tracer(service_name))),
    ]
}

/// Build the providers on a plain thread. `reqwest`'s blocking client panics when
/// it is built inside a Tokio runtime, and a Settings pane's save is an async
/// command; a thread of its own makes the caller's context irrelevant. A panic in
/// there is local only, never the caller's.
fn build_off_thread(
    installed: &Installed,
    headers: &HashMap<String, String>,
    endpoint: &str,
) -> Option<Providers> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                build_providers(
                    installed.service_name,
                    installed.service_version,
                    headers,
                    Some(endpoint),
                )
            })
            .join()
            .unwrap_or_default()
    })
}

/// Both providers, or neither. Signal endpoints, timeouts, batch sizing
/// (`OTEL_BLRP_*`/`OTEL_BSP_*`) and any static `OTEL_EXPORTER_OTLP_HEADERS` all
/// come from the SDK's own env handling; the helper's headers are stamped on last
/// by the client, so they win. `endpoint` is `None` for the environment's own
/// exporter, leaving the address to the SDK as well, and `Some` only for one a
/// Settings pane supplied, which no variable names.
fn build_providers(
    service_name: &'static str,
    service_version: &'static str,
    headers: &HashMap<String, String>,
    endpoint: Option<&str>,
) -> Option<Providers> {
    use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};

    // An empty endpoint parses as a relative URI, so it would build an exporter
    // that can never send. Local only is the honest answer.
    if endpoint.is_some_and(|base| base.trim().is_empty()) {
        return None;
    }
    ensure_crypto_provider();
    let resource = Resource::builder_empty()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new(SERVICE_VERSION, service_version))
        .build();
    let client = |timeout_var| bearer::HeaderClient::new(bearer::timeout(timeout_var), headers);

    let mut log_builder = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_http_client(client("OTEL_EXPORTER_OTLP_LOGS_TIMEOUT")?);
    let mut span_builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(client("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT")?);
    if let Some(base) = endpoint {
        log_builder = log_builder.with_endpoint(signal_url(base, "/v1/logs"));
        span_builder = span_builder.with_endpoint(signal_url(base, "/v1/traces"));
    }
    let log_exporter = log_builder.build().ok()?;
    let span_exporter = span_builder.build().ok()?;

    Some(Providers {
        logs: SdkLoggerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(log_exporter)
            .build(),
        traces: SdkTracerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(span_exporter)
            .build(),
    })
}

/// The developer's view: `RUST_LOG`, defaulting to `info`, never allow-listed.
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// How much of the exporter's own reporting reaches stderr.
struct ExporterNoise {
    last: AtomicU64,
    start: Instant,
}

impl ExporterNoise {
    const NEVER: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            last: AtomicU64::new(Self::NEVER),
            start: Instant::now(),
        }
    }

    /// Anything but the exporter passes untouched. The exporter's own debug lines
    /// carry the collector's response body, which upstream itself notes may echo a
    /// token back, so nothing below INFO gets through; what is left is worth one
    /// line a minute, not one per dropped event.
    fn admits(&self, target: &str, level: tracing::Level) -> bool {
        if !target.starts_with("opentelemetry") {
            return true;
        }
        if level > tracing::Level::INFO {
            return false;
        }
        let now = self.start.elapsed().as_secs();
        let previous = self.last.load(Ordering::Relaxed);
        let due = previous == Self::NEVER || now.saturating_sub(previous) >= 60;
        if due {
            self.last.store(now, Ordering::Relaxed);
        }
        due
    }
}

/// The TRACE hint is load-bearing: `And::max_level_hint` is the minimum over two
/// `Option`s and `None` sorts below `Some`, so an unhinted filter here would erase
/// `EnvFilter`'s hint and drop the global maximum to TRACE, evaluating every
/// `debug!`/`trace!` callsite in the process on every hit.
fn exporter_noise_filter<S: 'static>() -> impl Filter<S> + 'static {
    let noise = ExporterNoise::new();
    dynamic_filter_fn(move |meta: &Metadata<'_>, _: &Context<'_, S>| {
        noise.admits(meta.target(), *meta.level())
    })
    .with_callsite_filter(|_| tracing::subscriber::Interest::sometimes())
    .with_max_level_hint(LevelFilter::TRACE)
}

/// rustls needs a process-wide provider and reqwest panics without one under
/// `rustls-no-provider`, so every path that builds a client calls this first.
/// It is a no-op once anything has installed one, which keeps the choice the
/// application's wherever the application has made it.
pub(crate) fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SimpleLogProcessor};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};
    use serial_test::serial;

    const ALLOW: &[&str] = &["allowed_target"];

    /// SAFETY: every test that touches the environment is `#[serial]`, and no
    /// other test in this crate reads it.
    fn set_env(pairs: &[(&str, Option<&str>)]) {
        for (name, value) in pairs {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    fn clear_env() {
        set_env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
            ("OTEL_EXPORTER_OTLP_HEADERS_HELPER", None),
        ]);
    }

    /// The real layer stack over in-memory exporters, so a test sees exactly what
    /// the OTLP layers would have exported.
    fn in_memory() -> (Providers, InMemoryLogExporter, InMemorySpanExporter) {
        let logs = InMemoryLogExporter::default();
        let spans = InMemorySpanExporter::default();
        let providers = Providers {
            logs: SdkLoggerProvider::builder()
                .with_log_processor(SimpleLogProcessor::new(logs.clone()))
                .build(),
            traces: SdkTracerProvider::builder()
                .with_span_processor(SimpleSpanProcessor::new(spans.clone()))
                .build(),
        };
        (providers, logs, spans)
    }

    /// A `Guard` over a locally installed stack, so a swap can be driven without
    /// claiming the process's one global subscriber. `from_env` is the exporter
    /// the environment supplied, if any.
    fn guard_over(reload: OtlpHandle, from_env: Option<Exporter>) -> Guard {
        Guard {
            providers: Mutex::new(None),
            retiring: Mutex::new(None),
            installed: Some(Installed {
                reload,
                service_name: "test-service",
                service_version: "0.0.0",
            }),
            from_env: from_env.is_some(),
            exporter: Mutex::new(from_env),
        }
    }

    #[test]
    #[serial]
    fn no_endpoint_means_stderr_only() {
        clear_env();
        assert!(matches!(
            plan(Exporter::from_env().as_ref()),
            Plan::LocalOnly { .. }
        ));
        assert_eq!(Exporter::from_env(), None);
    }

    #[test]
    #[serial]
    fn a_helper_that_fails_means_stderr_only() {
        clear_env();
        set_env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://127.0.0.1:9/")),
            ("OTEL_EXPORTER_OTLP_HEADERS_HELPER", Some("exit 3")),
        ]);
        assert!(matches!(
            plan(Exporter::from_env().as_ref()),
            Plan::LocalOnly { .. }
        ));
        clear_env();
    }

    /// `init` is global and runs once per process, so this is the one test that
    /// calls it; everything else drives `subscriber` under `with_default`.
    #[test]
    #[serial]
    fn a_dead_endpoint_neither_blocks_nor_panics_and_a_second_init_builds_nothing() {
        clear_env();
        set_env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://127.0.0.1:9/")),
            (
                "OTEL_EXPORTER_OTLP_HEADERS_HELPER",
                Some(r#"printf '{"Authorization":"Bearer test"}'"#),
            ),
        ]);
        let started = Instant::now();
        let guard = init("test-service", "0.0.0", ALLOW);
        assert!(
            lock(&guard.providers).is_some(),
            "the OTLP providers were built"
        );
        assert!(guard.exporter_is_from_env(), "the environment set it");
        assert_eq!(
            guard.exporter().map(|e| e.endpoint),
            Some("http://127.0.0.1:9/".to_owned())
        );
        tracing::info!(target: "allowed_target", "allowed");
        tracing::info!(target: "some_other_target", "denied");

        let second = init("test-service", "0.0.0", ALLOW);
        assert!(
            lock(&second.providers).is_none(),
            "a second init builds nothing"
        );

        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "init plus a dead-endpoint flush took {:?}",
            started.elapsed()
        );
        clear_env();
    }

    /// `And::max_level_hint` is a minimum over `Option`s and `None` sorts below
    /// `Some`, so dropping the noise filter's hint would put the global maximum at
    /// TRACE and cost every `debug!` callsite in the process a per-hit evaluation.
    #[test]
    #[serial]
    fn the_stderr_filter_keeps_its_level_hint() {
        set_env(&[("RUST_LOG", None)]);
        let (subscriber, _reload) = subscriber(&[], OtlpLayers::new());
        let hint = tracing::Subscriber::max_level_hint(&subscriber);
        assert_eq!(hint, Some(LevelFilter::INFO));
    }

    #[test]
    fn the_exporters_own_reporting_is_floored_and_rate_limited() {
        let noise = ExporterNoise::new();
        assert!(
            noise.admits("bragi", tracing::Level::DEBUG),
            "not the exporter"
        );
        assert!(
            !noise.admits("opentelemetry-otlp", tracing::Level::DEBUG),
            "a debug line can carry the collector's response body"
        );
        assert!(noise.admits("opentelemetry_sdk", tracing::Level::ERROR));
        assert!(
            !noise.admits("opentelemetry_sdk", tracing::Level::ERROR),
            "one line a minute, not one per dropped event"
        );
        assert!(
            noise.admits("bragi", tracing::Level::INFO),
            "still untouched"
        );
    }

    #[test]
    #[serial]
    fn the_allow_list_holds_for_both_events_and_spans() {
        let (providers, logs, spans) = in_memory();
        let (subscriber, _reload) = subscriber(ALLOW, otlp_layers("test-service", &providers));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "allowed_target", "kept");
            tracing::info!(target: "allowed_target::inner", "kept too");
            tracing::info!(target: "another_target", "dropped");
            tracing::debug!(target: "allowed_target", "below the floor");
            tracing::info_span!(target: "allowed_target", "kept span").in_scope(|| {});
            tracing::info_span!(target: "another_target", "dropped span").in_scope(|| {});
        });
        providers.logs.force_flush().expect("flush logs");
        providers.traces.force_flush().expect("flush spans");

        let exported = logs.get_emitted_logs().expect("logs");
        assert_eq!(exported.len(), 2, "{exported:#?}");
        let exported = spans.get_finished_spans().expect("spans");
        let names: Vec<_> = exported.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(names, ["kept span"], "{names:?}");
    }

    /// The swap is what lets a Settings pane repoint a running app, so the record
    /// after it must reach the new destination and only the new destination.
    #[test]
    #[serial]
    fn a_swap_sends_the_next_record_only_to_the_new_destination() {
        let (subscriber, reload) = subscriber(ALLOW, OtlpLayers::new());
        let guard = guard_over(reload, None);
        let (first, first_logs, first_spans) = in_memory();
        let (second, second_logs, second_spans) = in_memory();

        tracing::subscriber::with_default(subscriber, || {
            guard.install(Some(first), None);
            tracing::info!(target: "allowed_target", "before the swap");
            let reached = format!("{:?}", first_logs.get_emitted_logs().expect("logs"));
            assert!(reached.contains("before the swap"), "{reached}");
            guard.install(
                Some(second),
                Some(Exporter {
                    endpoint: "http://127.0.0.1:9".to_owned(),
                    headers_helper: None,
                }),
            );
            tracing::info!(target: "allowed_target", "after the swap");
            tracing::info_span!(target: "allowed_target", "after the swap span").in_scope(|| {});
        });

        let after = format!("{:?}", second_logs.get_emitted_logs().expect("logs"));
        assert!(after.contains("after the swap"), "{after}");
        assert!(!after.contains("before the swap"), "{after}");
        let before = format!("{:?}", first_logs.get_emitted_logs().expect("logs"));
        assert!(!before.contains("after the swap"), "{before}");

        let names: Vec<_> = second_spans.get_finished_spans().expect("spans");
        let names: Vec<_> = names.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(names, ["after the swap span"], "{names:?}");
        assert!(
            first_spans.get_finished_spans().expect("spans").is_empty(),
            "the retired tracer took nothing after the swap"
        );
        assert_eq!(
            guard.exporter().map(|e| e.endpoint),
            Some("http://127.0.0.1:9".to_owned())
        );
    }

    /// `reqwest::blocking::ClientBuilder::build` panics inside a Tokio runtime, and
    /// a Settings pane's save is an async command.
    #[test]
    #[serial]
    fn a_swap_from_inside_a_tokio_runtime_does_not_panic() {
        clear_env();
        let (subscriber, reload) = subscriber(ALLOW, OtlpLayers::new());
        let guard = guard_over(reload, None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                // Port 9 discards; nothing leaves the host and no flush waits on it.
                guard.set_exporter(Some(Exporter {
                    endpoint: "http://127.0.0.1:9".to_owned(),
                    headers_helper: None,
                }));
            });
        });
        assert!(
            lock(&guard.providers).is_some(),
            "the swap installed an exporter"
        );
    }

    /// The fleet's variables win over a pane: on a machine that sets them, a
    /// Settings pane is a read-only display.
    #[test]
    #[serial]
    fn an_exporter_from_the_environment_cannot_be_replaced_by_a_pane() {
        clear_env();
        set_env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://127.0.0.1:9/"))]);
        let from_env = Exporter::from_env().expect("the environment set one");
        let (subscriber, reload) = subscriber(ALLOW, OtlpLayers::new());
        let guard = guard_over(reload, Some(from_env.clone()));

        tracing::subscriber::with_default(subscriber, || {
            guard.set_exporter(Some(Exporter {
                endpoint: "http://127.0.0.1:10/".to_owned(),
                headers_helper: None,
            }));
        });

        assert!(guard.exporter_is_from_env());
        assert_eq!(guard.exporter(), Some(from_env));
        assert!(lock(&guard.providers).is_none(), "nothing was built");
        clear_env();
    }

    /// The client span carries the path but never the query string or an error
    /// string, because its target is force-allowed past the caller's allow-list
    /// and a Subsonic query carries the salted auth token.
    #[test]
    #[serial]
    fn a_failed_request_records_the_path_but_never_the_query_or_an_error() {
        let (providers, _, spans) = in_memory();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (subscriber, _reload) = subscriber(&[], otlp_layers("test-service", &providers));
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                // Port 1 on loopback refuses immediately; nothing leaves the host.
                let result = http_client()
                    .get("http://127.0.0.1:1/library/search?q=secret-term&t=token")
                    .send()
                    .await;
                assert!(result.is_err(), "the request must fail");
            });
        });
        providers.traces.force_flush().expect("flush spans");

        let exported = spans.get_finished_spans().expect("spans");
        let span = exported.first().expect("the client span was exported");
        let rendered = format!("{:?}", span.attributes);
        for forbidden in ["secret-term", "token", "error."] {
            assert!(!rendered.contains(forbidden), "{forbidden} in {rendered}");
        }
        assert!(rendered.contains("server.address"), "{rendered}");
        assert!(rendered.contains("/library/search"), "{rendered}");
    }

    #[test]
    fn a_signal_path_is_appended_to_the_endpoint_however_it_ends() {
        assert_eq!(
            signal_url("https://otlp.example/", "/v1/logs"),
            "https://otlp.example/v1/logs"
        );
        assert_eq!(
            signal_url("https://otlp.example", "/v1/logs"),
            "https://otlp.example/v1/logs"
        );
        assert_eq!(
            signal_url("https://otlp.example//", "/v1/logs"),
            "https://otlp.example//v1/logs",
            "only the one slash the SDK strips"
        );
    }
}
