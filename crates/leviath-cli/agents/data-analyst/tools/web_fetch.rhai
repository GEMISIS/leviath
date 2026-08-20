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
    // Name what actually went wrong. The old text guessed "too large (>1MB) or
    // blocked" for every failure, and both guesses were wrong for the two most
    // common causes: a transport fault the host layer already retried, and a
    // body this client could not decode. An agent told the wrong reason picks
    // the wrong recovery - and one that reads "blocked" concludes the source is
    // unreachable and cites it from memory instead.
    let msg = `${e}`;
    let lower = msg.to_lower();
    let why = if lower.contains("unreadable body") {
        "the response was not readable text (compressed, binary, or an encoding this client cannot decode). Try an API endpoint, an RSS/JSON feed, or a static mirror";
    } else if lower.contains("http2") || lower.contains("connection") || lower.contains("timed out") {
        "the connection failed even after retries, so the host is refusing this client or is down. Try a syndicated copy of the same content, or rely on the web_search snippets";
    } else if lower.contains("http 4") || lower.contains("http 5") {
        "the host answered with an error status. Check the URL, or try a syndicated copy";
    } else {
        "the page may exceed the engine's 1MB string limit, or the host blocked the request. Try a more specific page, or rely on the web_search snippets";
    };
    out = `[web_fetch could not retrieve ${params.url}: ${msg}. ${why}. Whatever this page says, this fetch did not read it - do not cite this URL or reconstruct its contents from memory.]`;
}
out
