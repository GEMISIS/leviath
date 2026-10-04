//! What the event stream says about a run that comes back: paged in after
//! being parked, or restored when the daemon starts. Only what is new goes
//! out; what the run had already announced (that it started, its title, the
//! spend thresholds it had passed, that it finished) is not said again.

use super::*;
use crate::restore::Resumable;
use crate::state::RunStatus;
use leviath_core::RegionKind;
use tokio::runtime::Handle;

fn host() -> WorldHost {
    let world = PipelineWorld::new(
        crate::providers::ProviderRegistry::new(),
        Arc::new(super::tests::NoTools),
        crate::inference_pool::InferencePoolConfig::new(),
        1,
        None,
        Handle::current(),
    );
    WorldHost::new(world)
}

fn spec() -> Arc<crate::spec::run_spec::RunSpec> {
    use crate::test_graph as g;
    let layout = g::layout(
        vec![g::region("conversation", RegionKind::Clearable, 10_000)],
        12_000,
    );
    let stage = crate::spec::graph::StageDef {
        model: g::model("script", "m"),
        ..g::stage("s")
    };
    g::both(g::graph(vec![stage], layout)).0
}

/// The run as its file holds it: named, having spent `priced_usd`, in
/// `status`.
fn resumable(priced_usd: f64, status: RunStatus) -> Resumable {
    let spec = spec();
    let mut state = crate::insert::initial_state(&spec);
    state.title = Some("Named".to_string());
    state.totals.spend.priced_usd = priced_usd;
    state.status = status;
    Resumable {
        spec,
        state,
        code: Default::default(),
        answer: None,
        asked: 0,
    }
}

fn drain(rx: &mut broadcast::Receiver<WorldEvent>) -> Vec<WorldEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The kinds of event in `events`, in order.
fn kinds(events: &[WorldEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            WorldEvent::Spawned { .. } => "spawned",
            WorldEvent::Renamed { .. } => "renamed",
            WorldEvent::Status { .. } => "status",
            WorldEvent::Tokens { .. } => "tokens",
            WorldEvent::Context { .. } => "context",
            WorldEvent::Spend { .. } => "spend",
            WorldEvent::Completed { .. } => "completed",
            _ => "other",
        })
        .collect()
}

fn spend_thresholds(events: &[WorldEvent]) -> Vec<f64> {
    events
        .iter()
        .filter_map(|e| match e {
            WorldEvent::Spend { threshold_usd, .. } => Some(*threshold_usd),
            _ => None,
        })
        .collect()
}

/// A daemon that starts and restores a run tells subscribers where the run
/// stands, and nothing it had already said: no spawn, no rename to the name
/// it had, no threshold it had passed. What happens after is said as usual.
#[tokio::test]
async fn a_restored_run_announces_only_what_is_new() {
    let mut host = host();
    host.set_spend_notify_usd(vec![1.0, 5.0, 25.0]);
    let mut rx = host.subscribe();
    let entity = crate::restore::resume(
        host.world_mut().world_mut(),
        resumable(6.0, RunStatus::Paused),
        crate::spec::env::Bindings::new(),
    );

    host.emit_events();
    let first = drain(&mut rx);
    assert_eq!(
        kinds(&first),
        vec!["status", "tokens", "context"],
        "where the run stands, and nothing it had said before: {first:?}"
    );

    let mut totals = *host
        .world
        .world()
        .get::<crate::persistence::TokenTotals>(entity)
        .unwrap();
    totals.cost.priced_usd = 30.0;
    host.world_mut()
        .world_mut()
        .entity_mut(entity)
        .insert(totals);
    host.world_mut()
        .world_mut()
        .get_mut::<RunMetadata>(entity)
        .unwrap()
        .title = Some("Renamed".to_string());
    host.emit_events();
    let next = drain(&mut rx);
    assert_eq!(spend_thresholds(&next), vec![25.0], "only the new crossing");
    assert_eq!(
        kinds(&next).iter().filter(|k| **k == "renamed").count(),
        1,
        "a new name is announced: {next:?}"
    );
}

/// A run restored already finished does not finish a second time: no
/// completion goes out, so the completion webhook is not fired again.
#[tokio::test]
async fn a_restored_finished_run_does_not_complete_again() {
    let mut host = host();
    let mut rx = host.subscribe();
    crate::restore::resume(
        host.world_mut().world_mut(),
        resumable(0.0, RunStatus::Complete),
        crate::spec::env::Bindings::new(),
    );
    host.emit_events();
    let events = drain(&mut rx);
    assert!(
        !kinds(&events).contains(&"completed"),
        "it finished before: {events:?}"
    );
    assert!(
        !kinds(&events).contains(&"spawned"),
        "it started before: {events:?}"
    );
}

/// A run parked and paged back in is the same run, carrying on: the page-in
/// says it is running again and nothing more.
#[tokio::test]
async fn a_run_paged_back_in_announces_only_what_is_new() {
    let mut host = host();
    host.set_spend_notify_usd(vec![1.0, 5.0]);
    host.set_reloader(Box::new(|_run_id, _purpose| {
        Box::pin(async {
            let place: PlacePage = Box::new(|world: &mut PipelineWorld| {
                let entity = crate::restore::resume(
                    world.world_mut(),
                    resumable(6.0, RunStatus::Paused),
                    crate::spec::env::Bindings::new(),
                );
                Ok(world.own_agent(entity))
            });
            Ok(place)
        })
    }));
    let mut rx = host.subscribe();
    let spec = spec();
    let entity = crate::insert::insert(
        host.world_mut().world_mut(),
        spec.clone(),
        crate::spec::env::Bindings::new(),
        &crate::insert::initial_state(&spec),
    );
    let run_id = spec.run_id.to_string();

    host.emit_events();
    assert!(
        kinds(&drain(&mut rx)).contains(&"spawned"),
        "a new run starts"
    );
    let mut totals = *host
        .world
        .world()
        .get::<crate::persistence::TokenTotals>(entity)
        .unwrap();
    totals.cost.priced_usd = 6.0;
    host.world_mut()
        .world_mut()
        .entity_mut(entity)
        .insert(totals);
    host.world_mut()
        .world_mut()
        .get_mut::<RunMetadata>(entity)
        .unwrap()
        .title = Some("Named".to_string());
    host.emit_events();
    let said = drain(&mut rx);
    assert_eq!(spend_thresholds(&said), vec![1.0, 5.0]);
    assert!(kinds(&said).contains(&"renamed"));

    let (reply, paused) = tokio::sync::oneshot::channel();
    host.handle(ControlOp::Pause {
        run_id: run_id.clone(),
        reply,
    });
    assert!(paused.await.unwrap());
    let mut wm = crate::pipeline::PersistWatermark::default();
    wm.stamp_status(leviath_core::run_meta::RunStatus::Paused);
    host.world_mut().world_mut().entity_mut(entity).insert(wm);
    host.emit_events();
    assert!(host.parked.contains_key(&run_id), "parked");
    drain(&mut rx);

    let (reply, resumed) = tokio::sync::oneshot::channel();
    host.handle(ControlOp::Resume {
        run_id: run_id.clone(),
        reply,
    });
    assert_eq!(resumed.await.unwrap(), Ok(true));
    host.emit_events();
    let back = drain(&mut rx);
    assert_eq!(
        kinds(&back),
        vec!["status", "tokens", "context"],
        "running again, and nothing it had said before: {back:?}"
    );
}
