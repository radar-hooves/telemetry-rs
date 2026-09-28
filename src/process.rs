//! The process's own RSS and CPU, sampled periodically.
//!
//! A process's own resource use is never a listening history or a dictation
//! string — it is a fact about the binary, identical in shape whatever the
//! application is doing — so this target is force-allowed exactly as
//! `client::TARGET` and `panic::TARGET` are.
//!
//! [`sample_process_metrics`] is a future, not a spawned task: this crate
//! spawns nothing itself (`http_client` hands back a client rather than
//! driving requests, and `Guard::set_exporter` documents reaching it through
//! `spawn_blocking`), so the caller puts this future on their own runtime —
//! `tauri::async_runtime::spawn(telemetry::sample_process_metrics())` for a
//! Tauri app — exactly as they would their own background work.

use std::time::Duration;

use sysinfo::{Pid, System};

/// This crate's own force-allowed target — see the module doc.
pub(crate) const TARGET: &str = "telemetry::process";

/// How often the process is re-sampled. Long enough that a always-on sampler
/// costs nothing worth measuring; short enough that "is it a memory hog" is
/// answered from the last few minutes, not the last few hours.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Sample this process's RSS and CPU every 60 seconds and emit them as one
/// event on [`TARGET`], forever. Never returns; the caller spawns it once,
/// at startup, on their own async runtime.
///
/// Silently does nothing if the current process id cannot be read — a
/// platform sysinfo does not support, never a reason to fail startup over a
/// resource-use sampler.
pub async fn sample_process_metrics() {
    let Ok(pid) = sysinfo::get_current_pid() else {
        tracing::debug!("telemetry: could not read this process's own pid; not sampling");
        return;
    };
    let mut system = System::new();
    // Seed a first reading so the CPU percentage on the first emitted event is
    // computed over the interval below, not over an unknown span before this
    // function was ever called.
    refresh(&mut system, pid);
    loop {
        tokio::time::sleep(SAMPLE_INTERVAL).await;
        refresh(&mut system, pid);
        let Some(process) = system.process(pid) else {
            tracing::debug!("telemetry: this process is gone from sysinfo's own table");
            continue;
        };
        tracing::info!(
            target: TARGET,
            rss_bytes = process.memory(),
            cpu_percent = process.cpu_usage(),
            "process_metrics"
        );
    }
}

fn refresh(system: &mut System, pid: Pid) {
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry;

    #[derive(Default)]
    struct Captured(Arc<Mutex<Vec<(&'static str, String)>>>);

    struct Recorder(String);

    impl Visit for Recorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
        fn record_f64(&mut self, field: &Field, value: f64) {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut recorder = Recorder(String::new());
            event.record(&mut recorder);
            self.0
                .lock()
                .unwrap()
                .push((event.metadata().target(), recorder.0));
        }
    }

    /// One tick of the sampler's own loop body, run directly rather than
    /// through the real 60s sleep: proves the event's shape without a
    /// minute-long test.
    #[tokio::test]
    async fn one_sample_carries_rss_and_cpu_on_the_force_allowed_target() {
        let captured = Captured::default();
        let events = captured.0.clone();
        let dispatch = tracing::Dispatch::new(registry().with(captured));
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let pid = sysinfo::get_current_pid().expect("this process has a pid");
        let mut system = System::new();
        refresh(&mut system, pid);
        refresh(&mut system, pid);
        let process = system.process(pid).expect("this process is in the table");
        tracing::info!(
            target: TARGET,
            rss_bytes = process.memory(),
            cpu_percent = process.cpu_usage(),
            "process_metrics"
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let (target, fields) = &events[0];
        assert_eq!(*target, TARGET);
        assert!(fields.contains("rss_bytes="), "{fields}");
        assert!(fields.contains("cpu_percent="), "{fields}");
    }
}
