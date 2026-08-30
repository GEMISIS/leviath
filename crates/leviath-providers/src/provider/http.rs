//! The deadlines a provider call runs under, and the HTTP client that carries
//! it.
//!
//! Separated from "what a provider is" because it is a different subject and
//! because the two together put the module past the structure limit. Everything
//! here is shared: one client builder, one precedence order for the timeouts, so
//! a provider cannot quietly acquire its own.

/// Default wall-clock deadline, in seconds, for a single inference call when a
/// stage doesn't set its own `request_timeout_secs`.
///
/// This is the one unified inference timeout. It bounds every provider call the
/// same way: HTTP providers apply it as a per-request total timeout (see
/// [`apply_request_timeout`]), the claude-code provider as its subprocess
/// timeout, and the dispatch layer as the `RetryPolicy.job_timeout` backstop
/// that frees the pool slot even if a provider's own timer is defeated (e.g. by
/// trickle keep-alive). 15 minutes is generous for any real inference -
/// including large-prompt Anthropic cache creation, which can take several
/// minutes to first byte - while still guaranteeing a hung call cannot run
/// forever. A per-stage `[stages.<name>.model] request_timeout_secs` overrides
/// it, so a slow stage can wait longer and a fast one can fail sooner.
///
/// It is also [`build_http_client`]'s fallback when the config sets no global
/// `request_timeout_secs`, so side-channel calls that bypass the dispatch
/// backstop (title generation, compaction) are still bounded - a hung one
/// held its pool permit forever, observed live against Gemini. Precedence,
/// most specific wins: stage `request_timeout_secs` (per-request, overrides
/// the client's) > config `request_timeout_secs` (client-level) > this
/// default (client-level, only when the config is unset - it can never cap a
/// configured value).
pub const DEFAULT_INFERENCE_TIMEOUT_SECS: u64 = 900;

/// The deadline for a call that is *about* inference rather than inference
/// itself: counting a prompt's tokens, listing what a provider serves.
///
/// The 15-minute inference ceiling is the wrong number for these by two orders
/// of magnitude. Nothing generates here, so the whole answer
/// is one small body that either comes back promptly or is not coming, and both
/// callers already cope with not getting one - `count_tokens` falls back to the
/// local heuristic, a failed model list is reported and the provider is asked
/// again later.
///
/// Getting it wrong is not academic: `count_tokens` runs on the dispatch path,
/// *before* the request is sent and outside the `job_timeout` that bounds the
/// call itself, so a provider that accepts the connection and never answers
/// would otherwise freeze the run there for fifteen minutes with nothing in
/// flight to show for it.
pub const SIDE_CALL_TIMEOUT_SECS: u64 = 30;

/// Apply the per-call inference deadline to an outbound provider request.
///
/// When `request_timeout_secs` is `Some`, sets a hard per-request total timeout
/// (reqwest's `RequestBuilder::timeout`) so the call is aborted after that many
/// seconds regardless of connection state - this is what makes a per-stage
/// timeout *longer* than any global default actually take effect, and a shorter
/// one fail fast. When `None`, no per-request cap is added and the call is bound
/// only by the client-level timeout (if any) and the dispatch `job_timeout`
/// backstop.
pub fn apply_request_timeout(
    builder: reqwest::RequestBuilder,
    request_timeout_secs: Option<u64>,
) -> reqwest::RequestBuilder {
    match request_timeout_secs {
        Some(secs) => builder.timeout(std::time::Duration::from_secs(secs)),
        // No stage override: leave the builder alone so the client-level
        // timeout (the configured global, or the default) governs. Stamping
        // the default here would silently cap a LARGER configured global,
        // because reqwest's per-request timeout wins over the client's.
        None => builder,
    }
}

/// Build a `reqwest::Client` for talking to an LLM HTTP API.
///
/// All providers should use this instead of `Client::new()`. It applies:
/// - **`pool_max_idle_per_host(0)`** - never reuse an idle connection. A large
///   request sent over a *reused* pooled connection to `api.anthropic.com`
///   stalls indefinitely (the server never responds), 100% reproducibly on some
///   setups, while the *same* large request over a *fresh* connection succeeds
///   (confirmed via `curl`: a 40KB POST on a fresh connection returns HTTP 200,
///   and small requests, which don't trigger the stall, share the pool fine).
///   It is transport-independent - it reproduces over both HTTP/2 and HTTP/1.1 -
///   so forcing a fresh connection per request, not the protocol, is the fix.
///   The cost is a TLS handshake per request, negligible for the sequential
///   request/response calls these providers make. **This** is the real fix for
///   the never-responding-connection hang; the per-request timeout below is the
///   time bound on top of it.
/// - a `connect_timeout` so connection establishment can't hang; and TCP
///   keep-alive.
///
/// A *stall*/duration bound is deliberately **not** set on the client here.
/// Inference calls are bounded per-request instead (see [`apply_request_timeout`]
/// and `DEFAULT_INFERENCE_TIMEOUT_SECS`) so each stage can pick its own deadline,
/// with the dispatch `job_timeout` as the final backstop. `timeout_secs`
/// (`InferenceRequest::request_timeout_secs`) still applies an optional
/// client-level hard cap on total request duration for callers that set it; a
/// per-request timeout, when present, overrides it.
/// Whether a redirect keeps the request on the origin it started from.
///
/// Split out so the decision is a plain function the tests can drive, rather
/// than a closure only reachable through a real redirect.
fn same_origin_hop(attempt: &reqwest::redirect::Attempt<'_>) -> bool {
    attempt.previous().last().is_some_and(|prev| {
        prev.scheme() == attempt.url().scheme()
            && prev.host_str() == attempt.url().host_str()
            && prev.port_or_known_default() == attempt.url().port_or_known_default()
    })
}

/// The error `reqwest` reports when a client cannot be built, re-exported for
/// the same reason as [`HttpClient`].
pub use reqwest::Error as HttpError;

/// An [`HttpError`] instance, for tests that need to drive a failure path.
///
/// Constructing one otherwise is impossible: `reqwest::Error` has no public
/// constructor, and client construction cannot be made to fail from the
/// outside - a malformed `HTTPS_PROXY` is ignored rather than rejected, which
/// was measured rather than assumed. Handing the builder a string that is not a
/// URL produces one without any I/O. (An unknown *scheme* does not: reqwest
/// accepts it here and only fails on send.)
pub fn malformed_url_error() -> HttpError {
    reqwest::Client::builder()
        .build()
        .expect("a builder with no options set always yields a client")
        .get("https://[")
        .build()
        .expect_err("reqwest rejects a string that is not a URL")
}

/// The HTTP client providers hold, re-exported so callers can name the type
/// without taking a direct `reqwest` dependency of their own.
pub use reqwest::Client as HttpClient;

/// Builds an outbound HTTPS client for a given request timeout.
///
/// Injected so the failure path is reachable. `reqwest` will not fail to build a
/// client in any environment a test can arrange - a malformed `HTTPS_PROXY` is
/// ignored rather than rejected, which was measured, not assumed - so without a
/// seam the error arm would be unreachable code that no test could prove and the
/// coverage gate could not accept.
pub type HttpClientFactory<'a> =
    &'a (dyn Fn(Option<u64>) -> std::result::Result<reqwest::Client, reqwest::Error> + Send + Sync);

/// The HTTP client every provider talks through.
///
/// Redirects are capped and confined to the origin the request started on. That
/// second part is the load-bearing one: reqwest strips `Authorization` across
/// origins by itself but leaves custom headers alone, and the provider keys
/// travel as `x-api-key` and `x-goog-api-key`. A redirect to another host would
/// hand them over.
///
/// `timeout_secs` of `None` leaves the request untimed, which is what a
/// streaming call needs - a long generation is not a stalled one.
pub fn build_http_client(
    timeout_secs: Option<u64>,
) -> std::result::Result<reqwest::Client, reqwest::Error> {
    outbound_builder(timeout_secs).build()
}

/// The same client, pinned to HTTP/1.1.
///
/// Some origins negotiate HTTP/2 over ALPN and then fail every stream on it.
/// `investors.cerebras.ai` is one, measured: `curl` succeeds over either
/// protocol, while this stack gets `http2 error: stream error received:
/// unexpected internal error encountered` on **every** attempt, and both of
/// that host's pages were primary sources a research run ended up citing
/// without ever reading. A per-request `Version::HTTP_11` does not help,
/// because ALPN picks the protocol during the TLS handshake and the request
/// version is only a hint by then - it takes a separate client.
///
/// Built once and used only as a retry path, so the ordinary case still gets
/// HTTP/2 multiplexing.
pub fn build_http1_client(
    timeout_secs: Option<u64>,
) -> std::result::Result<reqwest::Client, reqwest::Error> {
    outbound_builder(timeout_secs).http1_only().build()
}

/// How many idle connections the side-call client keeps per host.
///
/// Two, not one: a count call can be in flight for one lane while another
/// lane's finishes, and a pool of one would open a fresh connection for the
/// second every time they overlap.
const SIDE_CALL_POOL_IDLE_PER_HOST: usize = 2;

/// How long an idle side-call connection is kept before it is closed.
const SIDE_CALL_POOL_IDLE_SECS: u64 = 30;

/// The client for a provider's small, frequent side calls - the token count
/// the window guard makes before a large request goes out.
///
/// The inference client above deliberately never reuses a connection, because
/// a *large* request over a reused connection to `api.anthropic.com` stalls
/// (see [`build_http_client`]). A count call is the opposite shape: a few
/// kilobytes, answered in milliseconds, and made before every request that is
/// big enough to be worth measuring. Paying a TLS handshake for each of those
/// was most of what the call cost, so this one keeps a couple of idle
/// connections per host for half a minute and reuses them.
///
/// One per process rather than one per provider, because the pool is keyed by
/// host anyway and a provider has no reason to hold its own copy. Built on
/// first use; the same builder options as the inference client otherwise, so
/// the origin-confined redirect policy that keeps the API keys at home applies
/// here too.
pub fn side_call_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        outbound_builder(Some(SIDE_CALL_TIMEOUT_SECS))
            .pool_max_idle_per_host(SIDE_CALL_POOL_IDLE_PER_HOST)
            .pool_idle_timeout(std::time::Duration::from_secs(SIDE_CALL_POOL_IDLE_SECS))
            .build()
            .expect("the side-call client builds from fixed options, like the inference client")
    })
}

/// The builder both outbound clients share.
///
/// Visible to the provider tests so one can swap in a resolver and still get
/// every other setting production uses.
pub(super) fn outbound_builder(timeout_secs: Option<u64>) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        // Follow a redirect only while it stays on the host the key was meant
        // for. reqwest strips `Authorization` across origins by itself, but not
        // a custom header - and the provider keys travel as `x-api-key`
        // (Anthropic) and `x-goog-api-key` (Gemini), which it would carry
        // straight to whatever a redirect named. `base_url` is user-configured
        // and legitimately points at loopback for Ollama, so this is an origin
        // check rather than `leviath-net`'s SSRF policy, which would
        // refuse that.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            match same_origin_hop(&attempt) && attempt.previous().len() <= 5 {
                true => attempt.follow(),
                // `stop` rather than `error`: the 3xx comes back as an ordinary
                // response and fails the status check downstream, which is one
                // error path instead of two.
                false => attempt.stop(),
            }
        }))
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(
            timeout_secs.unwrap_or(DEFAULT_INFERENCE_TIMEOUT_SECS),
        ))
}
