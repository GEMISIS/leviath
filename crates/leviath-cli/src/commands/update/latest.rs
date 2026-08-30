//! "Is there anything newer?" - the question `GET /api/update` cannot answer on
//! its own.
//!
//! [`crate::commands::update::plan`] works out *how* this copy would update and
//! deliberately makes no network call, so the route built on it can say what to
//! type and not whether it is worth typing. Without this, a console is left
//! comparing the daemon's version against a number baked into the site at
//! deploy time, which only knows the stable channel and is as stale as the last
//! build - so the people most likely to want an update prompt, the ones on
//! `alpha` and `beta`, are exactly the ones it has to stay silent for.
//!
//! The answer comes from the GitHub releases this repo already publishes, one
//! tag per channel, rather than from the package manager that would install it.
//! Asking `brew` and `scoop` is the more obvious "forward to the right place",
//! and it fails on the half of the problem that matters: their output is a text
//! format per tool per platform, a `cargo install` copy has nothing to ask at
//! all, and the install script leaves no record - so Scoop and Windows would
//! answer `null`. One publish point answers for every channel, on every
//! platform, for every install method.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Channel;

/// Where the releases live. One tag per channel: `prod.yml` publishes the
/// version and `latest`, `beta.yml` publishes `beta`, `alpha.yml` publishes
/// `alpha`.
const RELEASES_API: &str = "https://api.github.com/repos/GEMISIS/leviath/releases/tags";

/// How long an answer is good for.
///
/// Long because the question is "should I mention an update", which nobody
/// needs answered to the minute, and because an unauthenticated GitHub client
/// gets 60 requests an hour per address - a daemon that asked on every page
/// load would spend that in a minute and start answering `null` to everyone.
pub const CHECK_TTL_SECS: u64 = 3600;

/// The release tag that carries the newest build on a channel.
///
/// Stable reads `latest` rather than a version tag: the version is the thing
/// being looked up, so a tag named after it cannot be the way in.
fn tag_for(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "latest",
        Channel::Beta => "beta",
        Channel::Alpha => "alpha",
    }
}

/// What the last check found, or that it has not run.
///
/// Every field is optional together on purpose. "The check has not happened
/// yet", "it is switched off" and "it failed" are the same answer to a console -
/// nothing to show - and a client that renders `null` honestly as "can't tell"
/// is already the behaviour the site falls back to for pre-release channels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LatestCheck {
    /// Newest version on this copy's channel, without a leading `v`.
    pub latest: Option<String>,
    /// Whether `latest` is newer than the running build.
    pub update_available: Option<bool>,
    /// When the answer was obtained, unix seconds, so a console can say how
    /// fresh it is rather than presenting an hour-old answer as current.
    pub checked_at: Option<u64>,
}

impl LatestCheck {
    /// Whether this answer is old enough to be worth replacing.
    ///
    /// An unchecked cache is stale by definition, which is what makes the first
    /// request kick off a refresh instead of waiting for a timer.
    pub(crate) fn is_stale(&self, now: u64, ttl: u64) -> bool {
        match self.checked_at {
            Some(at) => now.saturating_sub(at) >= ttl,
            None => true,
        }
    }
}

/// How the check reaches the network, injected so the parsing and the comparing
/// can be tested without one.
///
/// Returns the response body. Any failure is the same as no answer: this
/// question is never worth surfacing an error for, because the honest rendering
/// of "the update check failed" and of "the update check has not run" are the
/// same sentence.
pub(crate) type ReleaseFetcher = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// Ask GitHub what the newest release on `channel` is, and compare it to
/// `running`.
///
/// `running` is the version this binary reports, so the comparison is against
/// the copy actually being asked about rather than against whatever the caller
/// last saw.
pub(crate) fn check_with(
    channel: Channel,
    running: &str,
    fetch: &ReleaseFetcher,
    now: u64,
) -> LatestCheck {
    let url = format!("{RELEASES_API}/{}", tag_for(channel));
    let Ok(body) = fetch(&url) else {
        return LatestCheck::default();
    };
    let Some(latest) = release_version(&body) else {
        return LatestCheck::default();
    };
    let update_available = is_newer(&latest, running);
    LatestCheck {
        update_available: Some(update_available),
        latest: Some(latest),
        checked_at: Some(now),
    }
}

/// The version a release payload names.
///
/// Reads `name` before `tag_name`: the channel tags are literally `alpha` and
/// `beta`, so for two of the three channels the tag is not a version at all and
/// only the release name carries one.
fn release_version(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    ["name", "tag_name"]
        .iter()
        .filter_map(|k| value.get(*k).and_then(|v| v.as_str()))
        .find_map(parse_version)
}

/// The version inside a release name, which is not always only a version:
/// `prod.yml` tags bare `0.4.2`, while a release may be titled `Leviath 0.4.2`
/// or `v0.4.2`.
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| {
            let mut parts = word.split('.');
            let numeric = |p: Option<&str>| {
                p.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            };
            numeric(parts.next()) && numeric(parts.next()) && parts.next().is_some()
        })
        .map(str::to_string)
}

/// Whether `latest` is a later version than `running`.
///
/// Compared field by field as numbers, because a string compare puts `0.10.0`
/// before `0.9.0` and would tell everyone on 0.10 to downgrade. A pre-release
/// suffix on either side is ignored for ordering and only breaks a tie: an
/// equal-numbered `0.5.0` is not an update over `0.5.0-alpha.1`, it is the
/// release that alpha was building towards, so it counts.
fn is_newer(latest: &str, running: &str) -> bool {
    let fields = |v: &str| -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (l, r) = (fields(latest), fields(running));
    match l.cmp(&r) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        // Same numbers: a plain version beats a pre-release of itself.
        std::cmp::Ordering::Equal => running.contains('-') && !latest.contains('-'),
    }
}

/// Unix seconds now, or 0 on a clock before the epoch.
///
/// 0 rather than an error because the only thing this stamps is "how fresh is
/// this answer", and a machine whose clock is that wrong has a bigger problem
/// than a stale update prompt.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The real network path: one GET, short timeout, body back as text.
///
/// A short timeout because nothing waits on this - the CLI prints its plan
/// either way and the daemon answers from a cache - so a slow answer is worth
/// less than a quick "can't tell". Blocking rather than async because the CLI
/// has no runtime and the daemon calls it off the request path.
///
/// GitHub refuses a request with no `User-Agent`, so it carries one naming the
/// program doing the asking, which is what their guidance asks for.
pub(crate) fn fetch_release(url: &str) -> Result<String, String> {
    fetch_release_capped(url, leviath_net::read_caps::JSON_BODY_CAP)
}

/// [`fetch_release`] with the body cap as a parameter, so a test can hit it
/// with a body of a few bytes rather than 64 MiB.
fn fetch_release_capped(url: &str, cap: usize) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .user_agent(concat!("leviath/", env!("CARGO_PKG_VERSION")))
        // `unwrap_or_default` rather than `?`: a builder given only a timeout
        // and a user-agent has no failure to report, so the error arm would be
        // a branch no request can reach. A default client would still make the
        // call, just without the timeout, and the caller treats a slow failure
        // and a fast one the same way.
        .build()
        .unwrap_or_default();
    let mut response = client.get(url).send().map_err(describe)?;
    // A 404 is an answer - that channel has published nothing - and it arrives
    // as a body carrying no version, which reads the same as any other unusable
    // answer. Nothing here separates the failures, because no caller renders
    // them differently.
    //
    // Read through `take` rather than `text()`, which buffers whatever the peer
    // sends: the same cap and message as every other buffered body the daemon
    // reads (see `leviath_net::read_caps`). One byte past the cap is read so
    // an over-long body is told apart from one that is exactly the cap.
    let peer = response
        .url()
        .host_str()
        .unwrap_or("an unnamed peer")
        .to_string();
    let mut body = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut response, limit), &mut body)
        .map_err(describe)?;
    if body.len() > cap {
        return Err(leviath_net::read_caps::BodyReadError::TooLarge { cap, peer }.to_string());
    }
    // The releases API answers UTF-8 JSON; lossy so a stray byte reads as a
    // body carrying no version rather than a second failure shape.
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Every failure in [`fetch_release`], flattened to its message.
///
/// One named function rather than a closure at each call site: the caller does
/// not distinguish these failures, and three identical closures would be three
/// branches that no reachable request can tell apart.
fn describe(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// How long to wait for GitHub before giving up and reporting nothing.
const FETCH_TIMEOUT_SECS: u64 = 5;

#[cfg(test)]
#[path = "latest_tests.rs"]
mod tests;
