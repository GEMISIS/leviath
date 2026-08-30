//! Rebuilding the telemetry pipeline when `[observability]` changes.
//!
//! The pipeline has two halves and both are swappable, so turning export on,
//! pointing it at a different collector, or turning it off again all take
//! effect on the next run rather than on the next `lev daemon restart`. The
//! sink is the world's `Telemetry` resource, replaced like any other resource;
//! the OTLP log bridge lives in a `tracing_subscriber` reload layer, which
//! `set_otel_layer` fills at boot and this refills (or empties) afterwards.
//!
//! Worth swapping rather than documenting a restart, because a sink stuck at
//! its boot value fails in the least readable way there is: you set
//! `enabled = true`, start a run to watch it, and nothing arrives - which looks
//! exactly like a collector that is not listening.
//!
//! What does **not** move is the base subscriber `logging::init` installs
//! before any config is read: the fmt layer, its stderr writer, and its
//! `info`/`debug` filter. Those come from `--verbose` on the process's own
//! command line, not from `[observability]`, and a `tracing` subscriber can
//! only be set once per process. Changing how verbose the daemon's own log is
//! still means restarting it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use leviath_core::config::ObservabilityConfig;
use leviath_core::telemetry::NoopSink;

/// How the OTLP log bridge reaches the process subscriber. Injected so a test
/// can watch what a rebuild asked for without racing other tests over the
/// process-wide reload handle, which only one of them can ever park.
///
/// A boxed closure rather than a bare `fn` pointer so an injected installer can
/// capture state a single test owns. A `fn` pointer forces the recorder to be a
/// `static`, which every test in the module then shares, and libtest runs those
/// tests on threads of one process: one test's reset lands between another's
/// write and its read, and the assertion sees a value no test ever asked for.
/// The daemon still installs exactly `logging::set_otel_layer`, on the same
/// schedule as before.
type InstallLayer = Box<dyn Fn(Option<leviath_telemetry::LogLayer>) -> bool + Send + Sync>;

/// Keeps the telemetry pipeline in step with `[observability]`.
pub struct TelemetryReload {
    /// The config the installed pipeline was built from. `None` before the
    /// first refresh, which is what makes the boot install go through the same
    /// path as every later change.
    applied: Mutex<Option<ObservabilityConfig>>,
    install_layer: InstallLayer,
}

impl TelemetryReload {
    /// One that forwards the OTLP log bridge to the process subscriber.
    pub fn for_daemon() -> Arc<Self> {
        Arc::new(Self::with_installer(Box::new(
            crate::logging::set_otel_layer,
        )))
    }

    /// One that reports its layer changes to `install_layer` instead.
    fn with_installer(install_layer: InstallLayer) -> Self {
        Self {
            applied: Mutex::new(None),
            install_layer,
        }
    }

    /// Rebuild and install the pipeline when `cfg` differs from the one in
    /// service. Returns whether anything was swapped.
    ///
    /// The outgoing sink is flushed before it is dropped, so spans and metrics
    /// already recorded reach the old collector rather than dying with it.
    ///
    /// A pipeline that cannot be built (an OTLP exporter that will not
    /// construct) leaves the no-op sink in place and warns, which is what boot
    /// did too: observability must never stop the work it observes. The config
    /// is recorded as applied either way, so a setting that cannot work is not
    /// retried on every spawn.
    pub fn refresh_into(
        &self,
        world: &mut leviath_runtime::PipelineWorld,
        cfg: &ObservabilityConfig,
    ) -> bool {
        let mut applied = self.lock();
        if applied.as_ref() == Some(cfg) {
            return false;
        }
        *applied = Some(cfg.clone());
        drop(applied);

        let ecs = world.world_mut();
        // Whatever is buffered belongs to the exporter on its way out. Read
        // rather than tested for: every world carries a sink from the moment
        // it is built, the no-op one until something replaces it.
        ecs.resource::<leviath_runtime::telemetry::Telemetry>()
            .0
            .force_flush();
        // Built before the swap: on OTLP this constructs an exporter, and a
        // failure has to leave the caller with a working (if silent) sink
        // rather than a half-installed one.
        let built = leviath_telemetry::build_sink(cfg);
        let sink: Arc<dyn leviath_core::telemetry::TelemetrySink> = match built {
            Some(built) => {
                // `None` for the stdout exporter clears a bridge a previous
                // OTLP config left behind, which is the point: the daemon's own
                // log lines must stop going to a collector the user turned off.
                (self.install_layer)(built.log_layer);
                built.sink
            }
            None => {
                (self.install_layer)(None);
                Arc::new(NoopSink)
            }
        };
        ecs.insert_resource(leviath_runtime::telemetry::Telemetry(sink));
        true
    }

    fn lock(&self) -> MutexGuard<'_, Option<ObservabilityConfig>> {
        self.applied.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::config::TelemetryExporterKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the injected installer was last asked for: 1 for a layer, 2 for
    /// clearing the slot, 0 for never called.
    ///
    /// One counter per test, handed back by [`reload`], rather than one
    /// `static` the whole module shares. libtest runs these tests on threads
    /// of a single process, so a shared counter is written by every test at
    /// once: whichever test resets it between another's install and that
    /// test's `load` makes the reader see a value nothing asked for. That is a
    /// real flake, not a theoretical one - it was reproduced here as
    /// `left: 0, right: 2` after the reset landed in the middle of a passing
    /// test. Nothing in the pipeline itself is process-wide, so there is
    /// nothing here to serialize: the counter simply belongs to the test.
    type Installs = Arc<AtomicUsize>;

    /// A reload whose installs land in a counter only the calling test holds.
    fn reload() -> (TelemetryReload, Installs) {
        let installs: Installs = Arc::new(AtomicUsize::new(0));
        let recorder = Arc::clone(&installs);
        let reload = TelemetryReload::with_installer(Box::new(move |layer| {
            recorder.store(if layer.is_some() { 1 } else { 2 }, Ordering::SeqCst);
            true
        }));
        (reload, installs)
    }

    fn cfg(enabled: bool, exporter: TelemetryExporterKind) -> ObservabilityConfig {
        ObservabilityConfig {
            enabled,
            exporter,
            endpoint: None,
            service_name: None,
        }
    }

    fn world() -> (tokio::runtime::Runtime, leviath_runtime::PipelineWorld) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let world = leviath_runtime::PipelineWorld::new(
            leviath_runtime::ProviderRegistry::new(),
            Arc::new(crate::daemon::tool_service::CliToolService::new()),
            leviath_runtime::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            runtime.handle().clone(),
        );
        (runtime, world)
    }

    /// The installed sink's identity, so a test can say "this is not the one
    /// that was there before" without the sink having to be downcastable.
    fn sink_ptr(world: &mut leviath_runtime::PipelineWorld) -> *const () {
        Arc::as_ptr(
            &world
                .world_mut()
                .get_resource::<leviath_runtime::telemetry::Telemetry>()
                .expect("the world always has a sink")
                .0,
        ) as *const ()
    }

    #[test]
    fn the_first_refresh_installs_and_a_repeat_of_it_does_not() {
        let (reload, _installs) = reload();
        let (_rt, mut world) = world();
        let enabled = cfg(true, TelemetryExporterKind::Stdout);
        assert!(reload.refresh_into(&mut world, &enabled));
        let after = sink_ptr(&mut world);

        assert!(
            !reload.refresh_into(&mut world, &enabled),
            "an unchanged [observability] must not rebuild the exporter"
        );
        assert_eq!(sink_ptr(&mut world), after);
    }

    /// `enabled = true` written after boot swaps the sink, so the next run
    /// reaches the collector rather than the no-op.
    #[test]
    fn turning_export_on_after_boot_swaps_the_sink() {
        let (reload, _installs) = reload();
        let (_rt, mut world) = world();
        assert!(reload.refresh_into(&mut world, &cfg(false, TelemetryExporterKind::Stdout)));
        let off = sink_ptr(&mut world);

        assert!(reload.refresh_into(&mut world, &cfg(true, TelemetryExporterKind::Stdout)));
        assert_ne!(
            sink_ptr(&mut world),
            off,
            "the run after the edit has to emit into the exporter the user just asked for"
        );
    }

    #[test]
    fn turning_export_off_puts_the_noop_sink_back_and_clears_the_bridge() {
        let (reload, installs) = reload();
        let (_rt, mut world) = world();
        reload.refresh_into(&mut world, &cfg(true, TelemetryExporterKind::Stdout));
        let on = sink_ptr(&mut world);

        assert!(reload.refresh_into(&mut world, &cfg(false, TelemetryExporterKind::Stdout)));
        assert_ne!(sink_ptr(&mut world), on);
        assert_eq!(
            installs.load(Ordering::SeqCst),
            2,
            "the daemon's own log lines must stop reaching a collector that was turned off"
        );
    }

    #[test]
    fn an_exporter_that_builds_nothing_leaves_a_working_silent_sink() {
        // `exporter = "none"` with `enabled = true` is the config-level way to
        // say "build no pipeline"; a failed OTLP construction reaches the same
        // arm, having warned.
        let (reload, installs) = reload();
        let (_rt, mut world) = world();
        assert!(reload.refresh_into(&mut world, &cfg(true, TelemetryExporterKind::None)));
        assert_eq!(installs.load(Ordering::SeqCst), 2);
        // Emitting into it is a no-op rather than a panic.
        world
            .world_mut()
            .get_resource::<leviath_runtime::telemetry::Telemetry>()
            .unwrap()
            .0
            .force_flush();
    }

    /// Moving to OTLP installs the daemon-log bridge as well as the sink, and
    /// moving off it takes the bridge back out. Port 9 (discard) is never
    /// connected until an export flush, which nothing here triggers.
    #[test]
    fn the_otlp_exporter_brings_the_daemon_log_bridge_with_it() {
        let (reload, installs) = reload();
        let (_rt, mut world) = world();
        let otlp = ObservabilityConfig {
            enabled: true,
            exporter: TelemetryExporterKind::Otlp,
            endpoint: Some("http://127.0.0.1:9".to_string()),
            service_name: Some("leviath-test".to_string()),
        };
        assert!(reload.refresh_into(&mut world, &otlp));
        assert_eq!(
            installs.load(Ordering::SeqCst),
            1,
            "the daemon's own log lines go to the collector the user just named"
        );

        assert!(reload.refresh_into(&mut world, &cfg(true, TelemetryExporterKind::Stdout)));
        assert_eq!(
            installs.load(Ordering::SeqCst),
            2,
            "and stop when the exporter they go through is no longer configured"
        );
    }

    #[test]
    fn a_changed_endpoint_is_a_change() {
        let (reload, _installs) = reload();
        let (_rt, mut world) = world();
        let mut first = cfg(true, TelemetryExporterKind::Stdout);
        reload.refresh_into(&mut world, &first);
        first.service_name = Some("leviath-b".to_string());
        assert!(
            reload.refresh_into(&mut world, &first),
            "a rename has to reach the exporter's resource attributes"
        );
    }

    #[test]
    fn for_daemon_starts_with_nothing_applied() {
        let reload = TelemetryReload::for_daemon();
        assert!(
            reload.lock().is_none(),
            "so the boot install goes through the same path as every later change"
        );
    }
}
