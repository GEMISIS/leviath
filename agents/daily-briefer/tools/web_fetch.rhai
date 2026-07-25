// @tool web_fetch
// @description Fetch a URL over HTTP(S) and return its readable text. HTML pages are stripped to prose (scripts/styles/tags removed, entities decoded); other content is returned as-is. Large pages are truncated; oversized/blocked requests return a diagnostic instead of failing the run.
// @param url string required "The URL to fetch (http or https)"
// @requires network

let out = "";
try {
    // A descriptive User-Agent avoids 403s from sites that block empty/default UAs.
    let body = http_get(params.url, #{ "User-Agent": "Leviath-Researcher/0.3 (+https://leviath.dev)" });

    // If the response looks like HTML, extract readable text so the model reads
    // prose instead of markup. (Content injected by client-side JS isn't in the
    // source and can't be recovered here.)
    let lo = body.to_lower();
    if lo.contains("<html") || lo.contains("<!doctype html") || lo.contains("<body") || lo.contains("<div") || lo.contains("</p>") || lo.contains("</a>") {
        body = html_to_text(body);
    }

    // Cap very large content so a single fetch can't blow the context budget.
    if body.len() > 12000 {
        out = body.sub_string(0, 12000) + "\n\n...[truncated — content was " + body.len() + " chars]";
    } else {
        out = body;
    }
} catch(e) {
    // Most commonly the page exceeds the engine's 1MB string limit, or the host
    // blocked the request. Report it so the agent can pick a smaller/other source.
    out = `[web_fetch could not retrieve ${params.url}: ${e}. The page may be too large (>1MB) or the request was blocked — try a more specific page or rely on the web_search snippets.]`;
}
out
