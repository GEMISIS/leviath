//! Reporting whether `config.toml` loads, over HTTP and over the websocket.
//!
//! The daemon's [`ConfigReloader`](crate::daemon::config_reload::ConfigReloader)
//! has always kept the last good config when a save did not parse, and this
//! server holds the same kind of reloader over the same file. What neither of
//! them did was say so: a user with a typo saw their edits stop taking effect
//! and had nothing to read that explained it.
//!
//! Two shapes come out of here. `GET /api/config` carries the answer as a
//! field on the config it returns, because the fact a client needs is "the
//! config you are reading is not the file on disk" and that belongs beside the
//! config. The websocket carries the *edges*, because a client that has drawn
//! a banner needs to be told when to take it down, and polling a config
//! endpoint to find out is the thing this exists to avoid.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::config_types::ConfigErrorInfo;
use super::events::ServerEvent;
use super::types::AppState;
use crate::config::ConfigFault;
use crate::daemon::config_reload::ConfigHealth;

/// How often the watcher re-checks the file.
///
/// A `stat` per interval, and nothing more while the mtime is unchanged. Two
/// seconds because this is a person editing a file in another window: fast
/// enough that the banner appears while they are still looking at the
/// terminal, slow enough to be free.
pub(super) const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// What a person is told is happening while the file will not load.
///
/// Written once, here, so the API, the dashboard and the CLI cannot end up
/// describing the same situation three slightly different ways.
pub(crate) const LAST_GOOD_NOTE: &str = "The running config is the last one that loaded; edits to this file take effect only \
     once it parses again.";

/// The API's view of a health snapshot: the error object, and the mtime of the
/// config actually in force.
pub(super) fn report(health: &ConfigHealth) -> (Option<ConfigErrorInfo>, Option<i64>) {
    let error = health.fault.as_ref().map(|fault| ConfigErrorInfo {
        kind: fault.kind.as_str().to_string(),
        path: fault.path.display().to_string(),
        message: fault.message.clone(),
        line: fault.line,
        column: fault.column,
        key: fault.key.clone(),
        since: health.since.map(unix_secs).unwrap_or_default(),
        note: LAST_GOOD_NOTE.to_string(),
    });
    (error, health.loaded_mtime.map(unix_secs))
}

/// The websocket frame for a health snapshot.
pub(super) fn event(health: &ConfigHealth) -> ServerEvent {
    let (error, config_mtime) = report(health);
    ServerEvent::ConfigHealth {
        healthy: health.is_healthy(),
        path: health.path.display().to_string(),
        error,
        config_mtime,
    }
}

/// A `SystemTime` as unix seconds, the way every other timestamp on this API
/// is spelled. A time before the epoch, which a filesystem can report for a
/// file with a nonsense mtime, reads as 0 rather than failing the request.
fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Watch the config file and broadcast a frame whenever its health changes.
///
/// Edge-triggered, never on a timer: a client holds a banner up from one frame
/// and takes it down on the next, and a subscriber that never sees one is
/// looking at a machine whose config has loaded the whole time it has been
/// connected. `GET /api/config` is what a client that connected mid-outage
/// asks.
///
/// Never returns; held behind the same abort-on-drop guard as the event loop.
pub(super) async fn watch_loop(state: AppState, interval: Duration) {
    let mut reported = state.config.health().fault;
    loop {
        tokio::time::sleep(interval).await;
        reported = broadcast_change(&state, &reported);
    }
}

/// One check: broadcast a frame if the answer is not what was last sent, and
/// return the answer now.
///
/// The comparison is on the whole fault rather than on "is it broken", because
/// a user fixing a syntax error and landing on a refused value never leaves the
/// broken state - and a client holding a banner would go on showing the line
/// and column of a problem that no longer exists.
///
/// Separate from the loop so the rule can be tested without a clock. Driving it
/// through the loop means racing the spawn against the first edit, and a test
/// that has to sleep to be right is a test that eventually is not.
fn broadcast_change(state: &AppState, reported: &Option<ConfigFault>) -> Option<ConfigFault> {
    let health = state.config.health();
    if &health.fault != reported {
        let _ = state.event_tx.send(event(&health));
    }
    health.fault
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::daemon::config_reload::ConfigReloader;
    use std::sync::Arc;

    /// Write `body` to `path` with an mtime strictly newer than any previous
    /// write, so a reload is observable even inside one clock tick.
    fn save(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap();
    }

    fn empty_config() -> String {
        toml::to_string(&Config::default()).unwrap()
    }

    /// A reloader over a real file, seeded the way boot seeds it.
    fn reloader(path: &std::path::Path) -> Arc<ConfigReloader> {
        Arc::new(ConfigReloader::new(
            path.to_path_buf(),
            Config::load_from_path_public(path).unwrap(),
        ))
    }

    #[test]
    fn a_healthy_config_reports_no_error_and_the_mtime_in_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let health = reloader(&path).health();

        let (error, mtime) = report(&health);
        assert!(error.is_none());
        assert!(mtime.is_some(), "a config that loaded has an mtime");
    }

    #[test]
    fn a_syntax_error_is_reported_with_its_place_and_the_last_good_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let reloader = reloader(&path);
        let (_, good_mtime) = report(&reloader.health());

        save(&path, "default_provider = \"anthropic\"\nbroken : :\n");
        let (error, mtime) = report(&reloader.health());

        let error = error.expect("a file that will not parse is an error");
        assert_eq!(error.kind, "parse");
        assert_eq!(error.line, Some(2));
        assert_eq!(error.column, Some(8));
        assert!(error.key.is_none());
        assert_eq!(error.path, path.display().to_string());
        assert!(error.since > 0, "the moment it broke is reported");
        assert_eq!(error.note, LAST_GOOD_NOTE);
        assert_eq!(
            mtime, good_mtime,
            "the mtime reported is the config in force, not the broken file"
        );
    }

    #[test]
    fn a_refused_value_is_reported_with_the_key_and_no_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let reloader = reloader(&path);

        save(
            &path,
            "[model_providers.local]\nkind = \"openai-compatible\"\n",
        );
        let (error, _) = report(&reloader.health());

        let error = error.expect("an endpoint with no address is an error");
        assert_eq!(error.kind, "validation");
        assert_eq!(error.key.as_deref(), Some("model_providers.local"));
        assert!(error.line.is_none() && error.column.is_none());
    }

    #[test]
    fn the_frame_carries_the_health_and_names_no_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let reloader = reloader(&path);

        let frame = event(&reloader.health());
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "config_health");
        assert_eq!(json["healthy"], true);
        assert!(json.get("error").is_none(), "no error when it loads");
        assert_eq!(frame.run_id(), "", "it is about the machine, not a run");
        assert!(
            !frame.is_for_run("run-1"),
            "a per-run subscription is not the place for it"
        );

        save(&path, "broken : :");
        let frame = event(&reloader.health());
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["healthy"], false);
        assert_eq!(json["error"]["kind"], "parse");
        assert_eq!(json["path"], path.display().to_string());
    }

    /// An mtime before the epoch is nonsense a filesystem can still hand back;
    /// it reports as 0 rather than taking the request down.
    #[test]
    fn a_time_before_the_epoch_reads_as_zero() {
        assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_secs(5)), 0);
        assert_eq!(unix_secs(UNIX_EPOCH + Duration::from_secs(7)), 7);
    }

    #[test]
    fn a_frame_goes_out_on_each_edge_and_on_neither_steady_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let state = super::super::testutil::state_with_config_at(&path);
        let mut rx = state.event_tx.subscribe();

        // Steady and loading: nothing to say.
        let reported = broadcast_change(&state, &None);
        assert!(reported.is_none());
        assert!(rx.try_recv().is_err(), "no frame while nothing changed");

        save(&path, "broken : :");
        let reported = broadcast_change(&state, &reported);
        assert!(reported.is_some(), "the file stopped loading");
        let frame = serde_json::to_value(rx.try_recv().expect("a frame on the edge")).unwrap();
        assert_eq!(frame["type"], "config_health");
        assert_eq!(frame["healthy"], false);
        assert_eq!(frame["error"]["kind"], "parse");

        // Still broken, in the same way, and already announced.
        let reported = broadcast_change(&state, &reported);
        assert!(rx.try_recv().is_err(), "a held state repeats no frame");

        // Still broken, for a different reason: a client holding a banner has
        // to be told, or it goes on showing a position that no longer exists.
        save(
            &path,
            "[model_providers.local]\nkind = \"openai-compatible\"\n",
        );
        let reported = broadcast_change(&state, &reported);
        let frame = serde_json::to_value(rx.try_recv().expect("a new reason is an edge")).unwrap();
        assert_eq!(frame["healthy"], false);
        assert_eq!(frame["error"]["kind"], "validation");
        assert_eq!(frame["error"]["key"], "model_providers.local");

        save(&path, &empty_config());
        let reported = broadcast_change(&state, &reported);
        assert!(reported.is_none(), "it loads again");
        let frame = serde_json::to_value(rx.try_recv().expect("recovery is an edge too")).unwrap();
        assert_eq!(frame["healthy"], true);
        assert!(frame.get("error").is_none());
    }

    /// The loop itself: it polls on its interval and puts a frame on the
    /// channel when the file changes under it. The *rule* it applies is tested
    /// above, without a clock; this is the wiring.
    #[tokio::test]
    async fn the_watch_loop_notices_the_file_changing_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &empty_config());
        let state = super::super::testutil::state_with_config_at(&path);
        let mut rx = state.event_tx.subscribe();

        let watcher = tokio::spawn(watch_loop(state, Duration::from_millis(5)));
        // Let the task reach its first `sleep`, which is past the baseline
        // read: break the file before that and there is no edge to see. The
        // test runtime is single-threaded, so one yield is enough.
        tokio::task::yield_now().await;
        save(&path, "broken : :");

        let frame = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the watcher sends a frame")
            .expect("the channel stays open");
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "config_health");
        watcher.abort();
    }
}
