// @tool web_fetch
// @description Fetch a URL over HTTP(S) and return its readable text. HTML pages are stripped to prose (scripts/styles/tags removed, entities decoded); other content is returned as-is. Large pages are truncated; oversized, blocked, or client-side-rendered pages return a diagnostic instead of failing the run.
// @param url string required "The URL to fetch (http or https)"
// @requires network

// Below this many characters, an HTML page that stripped to prose gave us a
// shell, not an article: the text lives in client-side JS that never ran here.
// Reddit strips to the single word "Reddit", and a model handed that will
// happily cite the discussion it remembers being there.
let MIN_HTML_PROSE = 200;

let out = "";
try {
    // A descriptive User-Agent avoids 403s from sites that block empty/default UAs.
    let body = http_get(params.url, #{ "User-Agent": "Leviath-Researcher/0.3 (+https://leviath.dev)" });

    // If the response looks like HTML, extract readable text so the model reads
    // prose instead of markup. (Content injected by client-side JS isn't in the
    // source and can't be recovered here.)
    let lo = body.to_lower();
    let was_html = lo.contains("<html") || lo.contains("<!doctype html") || lo.contains("<body") || lo.contains("<div") || lo.contains("</p>") || lo.contains("</a>");
    if was_html {
        body = html_to_text(body);
    }

    if was_html && body.trim().len() < MIN_HTML_PROSE {
        // Name it rather than returning the husk. A few words of navigation
        // chrome is indistinguishable from real content once it is sitting in
        // the sources region next to everything else.
        out = `[web_fetch got no readable content from ${params.url}: the page is HTML but strips to only ${body.trim().len()} characters, so its text is rendered client-side by JavaScript that does not run here. Whatever it says, this fetch did not read it - do not cite this URL or reconstruct its contents from memory. Try an API endpoint, an RSS/JSON feed, or a static mirror instead.]`;
    } else if body.len() > 12000 {
        // Cap very large content so a single fetch can't blow the context budget.
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
