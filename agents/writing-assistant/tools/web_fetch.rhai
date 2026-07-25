// @tool web_fetch
// @description Fetch the raw contents of a URL over HTTP(S) and return the response body as text. Large pages are truncated; pages over the engine's 1MB limit or blocked requests return a diagnostic message instead of failing the run.
// @param url string required "The URL to fetch (http or https)"
// @requires network

let out = "";
try {
    // A descriptive User-Agent avoids 403s from sites that block empty/default UAs.
    let body = http_get(params.url, #{ "User-Agent": "Leviath-Researcher/0.3 (+https://leviath.dev)" });
    // Cap very large pages so a single fetch can't blow the context budget.
    if body.len() > 12000 {
        out = body.sub_string(0, 12000) + "\n\n...[truncated — page was " + body.len() + " chars]";
    } else {
        out = body;
    }
} catch(e) {
    // Most commonly the page exceeds the engine's 1MB string limit, or the host
    // blocked the request. Report it so the agent can pick a smaller/other source.
    out = `[web_fetch could not retrieve ${params.url}: ${e}. The page may be too large (>1MB) or the request was blocked — try a more specific page or rely on the web_search snippets.]`;
}
out
