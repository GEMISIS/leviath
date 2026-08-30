//! Keeping the world the daemon runs in step with `[limits]` and `[title]`.
//!
//! The settings in this module are the ones the world is *built* with: pool
//! sizes, lane widths, watchdog timeouts, the circuit breaker, the retry
//! schedule, the fan-out ceiling, the listing window, the spend figures and
//! whether runs get titles. Every one of them was read once in
//! [`build_host`](crate::daemon::setup::build_host) and then fixed for the life
//! of the process, so editing any of them and starting a run did nothing at
//! all, with nothing to say so.
//!
//! One of those was worse than doing nothing. Spawn already reads a fresh
//! config, so turning `[title]` on made spawn mark the new run `PendingTitle` -
//! and the dispatch system, reading the boot-time `TitleSettings`, saw titling
//! switched off and quietly dropped the marker. The run was marked for a title
//! that could never be made. Applying the same config to the world before the
//! spawner reads it is what closes that: both halves now answer from one
//! document.
//!
//! The comparison is on the settings themselves rather than the file's mtime,
//! so a save that only changed a tool permission does not touch the pools. Each
//! setting says for itself whether it reaches a run already under way: a world
//! resource is read every tick, so the watchdogs, the breaker, the retry
//! schedule and the fan-out ceiling do; a pool or lane width applies to the
//! next request that asks for a slot, never to one already in flight.

use std::sync::{Arc, Mutex};

use leviath_runtime::PipelineWorld;
use leviath_runtime::host::HostSettings;
use leviath_runtime::interaction_hub::InteractionHub;

use crate::config::Config;

/// The settings this module owns, as of the last time they were applied.
///
/// Whole config sections rather than a hand-listed subset: a key added to
/// `[limits]` later is then compared for free, and the cost of comparing one
/// this module does not install is a re-apply that changes nothing.
#[derive(Clone, PartialEq)]
struct Applied {
    limits: crate::config::LimitsConfig,
    title: leviath_core::config::TitleConfig,
}

impl Applied {
    fn of(config: &Config) -> Self {
        Self {
            limits: config.limits.clone(),
            title: config.title.clone(),
        }
    }
}

/// Applies `[limits]` and `[title]` to a live world.
///
/// Built once at boot and consulted wherever the daemon has a freshly reloaded
/// config and the world in hand: the spawner, and the reloader that pages a run
/// back in.
pub struct LiveLimits {
    /// The prompt hub, which holds its own timeout rather than living in the
    /// world.
    hub: InteractionHub,
    /// The host's own settings, shared out of [`WorldHost::settings`].
    ///
    /// [`WorldHost::settings`]: leviath_runtime::host::WorldHost::settings
    host: HostSettings,
    /// What was applied last, or `None` before the first application, which is
    /// why boot applies everything.
    applied: Mutex<Option<Applied>>,
}

impl LiveLimits {
    /// A fresh applier over `hub` and `host`, which has applied nothing yet.
    pub fn new(hub: InteractionHub, host: HostSettings) -> Self {
        Self {
            hub,
            host,
            applied: Mutex::new(None),
        }
    }

    /// Put `config`'s limits and title settings into `world`, if they are not
    /// the ones already in it. Reports whether anything was applied.
    ///
    /// Cheap enough to call before every spawn: an unchanged config costs one
    /// comparison. The lock is held across the whole application so two spawns
    /// arriving together cannot interleave halves of two different configs.
    pub fn apply(&self, config: &Config, world: &mut PipelineWorld) -> bool {
        let next = Applied::of(config);
        let mut applied = leviath_core::sync::lock(&self.applied);
        if applied.as_ref() == Some(&next) {
            return false;
        }
        *applied = Some(next);

        // Concurrency. A raised limit is usable at once; a lowered one narrows
        // as the requests and batches in flight finish, so nothing running is
        // interrupted and no slot is taken back from work already under way.
        world.set_inference_pool_config(config.limits.inference_pools());
        world.set_tool_concurrency(config.limits.max_concurrent_tools);
        // Read when a request is assembled, so this reaches the next inference
        // any run makes rather than one already on the wire.
        world.set_stream_inference(config.limits.stream_inference);

        // World resources, every one of them read by a system on each tick, so
        // these reach the runs already in flight as well as the next one.
        let ecs = world.world_mut();
        ecs.insert_resource(leviath_runtime::pipeline::StallTimeout(
            config.limits.stall_timeout_secs,
        ));
        ecs.insert_resource(leviath_runtime::pipeline::WedgeTimeout(
            config.limits.wedge_timeout_secs,
        ));
        ecs.insert_resource(leviath_runtime::pipeline::CircuitPolicy {
            failures_before_open: config.limits.provider_failures_before_open,
            cooldown_secs: config.limits.provider_circuit_cooldown_secs,
        });
        ecs.insert_resource(leviath_runtime::pipeline::InferenceRetryTuning {
            max_attempts: config.limits.inference_retry_attempts,
            base_delay_ms: config.limits.inference_retry_base_ms,
        });
        // Read at every fan-out split, so a ceiling lowered mid-run stops the
        // next split rather than waiting for the next run.
        ecs.insert_resource(leviath_runtime::fanout::FanOutBudget(
            config.limits.max_agents_per_run,
        ));
        // The half of the title split that used to be stuck at boot: the
        // dispatcher and the spawner now read the same document.
        ecs.insert_resource(leviath_runtime::title::TitleSettings(config.title.clone()));

        // Read when a prompt opens, so a prompt already waiting keeps the
        // deadline it opened with.
        self.hub
            .set_timeout_secs(config.limits.interaction_timeout_secs);

        // The host's own settings, read once per re-drive or per event pass.
        self.host
            .set_dead_cycles_before_relief(config.limits.dead_cycles_before_relief);
        self.host
            .set_spend_notify_usd(config.limits.notify_spend_usd.clone());
        self.host
            .set_finished_retention_secs(config.limits.finished_retention_secs);
        true
    }
}

/// One applier for the daemon to share between its spawn and reload hooks.
pub fn for_daemon(hub: InteractionHub, host: HostSettings) -> Arc<LiveLimits> {
    Arc::new(LiveLimits::new(hub, host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::ProviderRegistry;
    use leviath_runtime::inference_pool::InferencePoolConfig;

    /// A bare world, with the tool lane at `tools` and the inference pools at
    /// whatever `PipelineWorld::new` is handed.
    fn world(runtime: &tokio::runtime::Runtime, tools: usize) -> PipelineWorld {
        let _guard = runtime.enter();
        PipelineWorld::new(
            ProviderRegistry::new(),
            Arc::new(crate::daemon::tool_service::CliToolService::new()),
            InferencePoolConfig::new().with_default(Some(1)),
            tools,
            None,
            runtime.handle().clone(),
        )
    }

    fn applier() -> LiveLimits {
        LiveLimits::new(InteractionHub::new(), HostSettings::default())
    }

    #[test]
    fn the_first_apply_installs_everything_and_a_repeat_does_nothing() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 8);
        let live = applier();
        let config = Config::default();
        assert!(live.apply(&config, &mut world), "boot applies");
        assert!(
            !live.apply(&config, &mut world),
            "an unchanged config must not touch the pools of a running daemon"
        );
    }

    #[test]
    fn the_inference_pool_limits_follow_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 8);
        let live = applier();
        let mut config = Config::default();
        config.limits.max_concurrent_inferences = Some(1);
        live.apply(&config, &mut world);
        assert_eq!(world.inference_pool_config().limit_for("m"), Some(1));

        config.limits.max_concurrent_inferences = Some(4);
        config
            .limits
            .max_concurrent_inferences_by_model
            .insert("m".to_string(), 2);
        config
            .limits
            .max_concurrent_inferences_by_provider
            .insert("slow".to_string(), 1);
        assert!(live.apply(&config, &mut world));
        let pools = world.inference_pool_config();
        assert_eq!(pools.limit_for("other"), Some(4), "the global fallback");
        assert_eq!(pools.limit_for("m"), Some(2), "the per-model override");
        assert_eq!(pools.provider_limit_for("slow"), Some(1));

        config
            .limits
            .max_concurrent_inferences_by_provider
            .remove("slow");
        assert!(live.apply(&config, &mut world));
        assert_eq!(
            world.inference_pool_config().provider_limit_for("slow"),
            None,
            "a cap the user deleted stops applying without a restart"
        );
    }

    #[test]
    fn the_tool_lane_widens_and_narrows_with_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 2);
        let live = applier();
        let mut config = Config::default();
        config.limits.max_concurrent_tools = 2;
        live.apply(&config, &mut world);
        assert_eq!(world.tool_concurrency(), 2);

        config.limits.max_concurrent_tools = 6;
        live.apply(&config, &mut world);
        assert_eq!(world.tool_concurrency(), 6, "widened without a restart");

        config.limits.max_concurrent_tools = 3;
        live.apply(&config, &mut world);
        assert_eq!(world.tool_concurrency(), 3, "and narrowed again");
    }

    #[test]
    fn the_watchdog_timeouts_follow_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = applier();
        let mut config = Config::default();
        config.limits.stall_timeout_secs = 60;
        config.limits.wedge_timeout_secs = 0;
        live.apply(&config, &mut world);

        config.limits.stall_timeout_secs = 5;
        config.limits.wedge_timeout_secs = 300;
        assert!(live.apply(&config, &mut world));
        assert_eq!(
            world
                .world()
                .resource::<leviath_runtime::pipeline::StallTimeout>()
                .0,
            5
        );
        assert_eq!(
            world
                .world()
                .resource::<leviath_runtime::pipeline::WedgeTimeout>()
                .0,
            300
        );
    }

    #[test]
    fn the_circuit_policy_and_retry_schedule_follow_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = applier();
        let mut config = Config::default();
        live.apply(&config, &mut world);

        config.limits.provider_failures_before_open = 9;
        config.limits.provider_circuit_cooldown_secs = 42;
        config.limits.inference_retry_attempts = 7;
        config.limits.inference_retry_base_ms = 250;
        assert!(live.apply(&config, &mut world));
        let policy = world
            .world()
            .resource::<leviath_runtime::pipeline::CircuitPolicy>();
        assert_eq!(policy.failures_before_open, 9);
        assert_eq!(policy.cooldown_secs, 42);
        let retry = world
            .world()
            .resource::<leviath_runtime::pipeline::InferenceRetryTuning>();
        assert_eq!(retry.max_attempts, 7);
        assert_eq!(retry.base_delay_ms, 250);
    }

    #[test]
    fn the_fan_out_budget_follows_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = applier();
        let mut config = Config::default();
        live.apply(&config, &mut world);
        assert_eq!(
            world
                .world()
                .resource::<leviath_runtime::fanout::FanOutBudget>()
                .0,
            0
        );

        config.limits.max_agents_per_run = 20;
        assert!(live.apply(&config, &mut world));
        assert_eq!(
            world
                .world()
                .resource::<leviath_runtime::fanout::FanOutBudget>()
                .0,
            20
        );
    }

    #[test]
    fn title_settings_follow_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = applier();
        let mut config = Config::default();
        config.title.enabled = false;
        live.apply(&config, &mut world);
        assert!(
            !world
                .world()
                .resource::<leviath_runtime::title::TitleSettings>()
                .0
                .enabled
        );

        config.title.enabled = true;
        config.title.provider = Some("anthropic".to_string());
        assert!(live.apply(&config, &mut world));
        let settings = world
            .world()
            .resource::<leviath_runtime::title::TitleSettings>();
        assert!(settings.0.enabled);
        assert_eq!(settings.0.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn the_host_settings_and_the_prompt_timeout_follow_the_config() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let hub = InteractionHub::new();
        let host = HostSettings::default();
        let live = LiveLimits::new(hub.clone(), host.clone());
        let mut config = Config::default();
        live.apply(&config, &mut world);

        config.limits.dead_cycles_before_relief = 3;
        config.limits.finished_retention_secs = 30;
        config.limits.notify_spend_usd = vec![25.0, 5.0];
        assert!(live.apply(&config, &mut world));
        assert_eq!(host.dead_cycles_before_relief(), 3);
        assert_eq!(host.finished_retention_secs(), 30);
        assert_eq!(*host.spend_notify_usd(), vec![5.0, 25.0]);
        // The hub reads its own field, so it is checked through the hub.
        assert_eq!(hub.timeout_secs(), config.limits.interaction_timeout_secs);
    }

    #[test]
    fn stream_inference_can_be_turned_off_without_a_restart() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = applier();
        let mut config = Config::default();
        live.apply(&config, &mut world);
        assert!(world.stream_inference());

        config.limits.stream_inference = false;
        assert!(live.apply(&config, &mut world));
        assert!(!world.stream_inference());
    }

    #[test]
    fn for_daemon_builds_one_that_has_applied_nothing_yet() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut world = world(&runtime, 1);
        let live = for_daemon(InteractionHub::new(), HostSettings::default());
        assert!(live.apply(&Config::default(), &mut world));
    }
}
