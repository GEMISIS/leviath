// @tool web_search
// @description Search the web and return a JSON list of {title, url, snippet} results. Uses Brave Search when BRAVE_API_KEY is set in the environment, otherwise a keyless Wikipedia search (works out of the box for any query).
// @param query string required "The search query"
// @param count integer optional "Maximum number of results to return (default 5)"
// @requires network

let n = if params.count == () { 5 } else { params.count };
let results = [];

// Prefer Brave Search when an API key is configured. env_var throws when the
// variable is unset, so probe it inside try/catch and fall back to keyless search.
let key = "";
try { key = env_var("BRAVE_API_KEY"); } catch(e) { key = ""; }

let got = false;
if key != "" {
    try {
        let url = `https://api.search.brave.com/res/v1/web/search?q=${encode_uri(params.query)}&count=${n}`;
        let data = parse_json(http_get(url, #{ "X-Subscription-Token": key, "Accept": "application/json" }));
        if data.web != () && data.web.results != () {
            for r in data.web.results {
                if results.len() >= n { break; }
                results.push(#{ title: r.title, url: r.url, snippet: r.description });
            }
            got = true;
        }
    } catch(e) {
        // Brave failed (bad key, rate limit, network) — fall through to keyless search.
        got = false;
    }
}

if !got {
    // Keyless fallback: Wikipedia's search API returns clean JSON for arbitrary
    // queries (no key required). Each page becomes a {title, url, snippet} result.
    let url = `https://en.wikipedia.org/w/rest.php/v1/search/page?q=${encode_uri(params.query)}&limit=${n}`;
    // Wikipedia requires a descriptive User-Agent (403 otherwise).
    let data = parse_json(http_get(url, #{ "User-Agent": "Leviath-Researcher/0.3 (+https://leviath.dev)", "Accept": "application/json" }));
    if data.pages != () {
        for p in data.pages {
            if results.len() >= n { break; }
            let snippet = if p.excerpt != () && p.excerpt != "" { p.excerpt } else if p.description != () { p.description } else { "" };
            // Wikipedia wraps matched terms in <span class="searchmatch"> tags — strip them.
            snippet.replace("<span class=\"searchmatch\">", "");
            snippet.replace("</span>", "");
            results.push(#{ title: p.title, url: `https://en.wikipedia.org/wiki/${p.key}`, snippet: snippet });
        }
    }
}

results
