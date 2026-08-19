// @tool web_search
// @description Search the web and return a JSON list of {title, url, snippet, date} results. Uses Brave Search when BRAVE_API_KEY is readable. Without it there is no search engine: the call falls back to a keyless Wikipedia lookup and says so in its result.
// @param query string required "The search query"
// @param count integer optional "Maximum number of results to return (default 5)"
// @param freshness string optional "Restrict results by age: pd (past day), pw (past week), pm (past month), py (past year), or YYYY-MM-DDtoYYYY-MM-DD. Use this whenever 'latest' or 'current' matters."
// @requires network

let n = if params.count == () { 5 } else { params.count };
let results = [];

// Probe BRAVE_API_KEY. env_var throws for two very different reasons and the
// caller has to tell them apart: the variable is genuinely unset, or it is set
// and the security layer refused it because the name looks like a credential
// (`[security] allow_env_vars`). Collapsing both into "" is what let this tool
// degrade to Wikipedia for a whole run without anyone noticing.
let key = "";
let key_state = "unset";
try {
    key = env_var("BRAVE_API_KEY");
    key_state = if key == "" { "unset" } else { "ok" };
} catch(e) {
    let msg = `${e}`;
    key_state = if msg.contains("[denied]") { "denied" } else { "unset" };
    key = "";
}

// Why no engine ran, phrased as the fix rather than the symptom. Empty when one did.
let engine_problem = "";

if key_state == "ok" {
    try {
        let url = `https://api.search.brave.com/res/v1/web/search?q=${encode_uri(params.query)}&count=${n}`;
        if params.freshness != () && params.freshness != "" {
            url += `&freshness=${encode_uri(params.freshness)}`;
        }
        let data = parse_json(http_get(url, #{ "X-Subscription-Token": key, "Accept": "application/json" }));
        if data.web != () && data.web.results != () {
            for r in data.web.results {
                if results.len() >= n { break; }
                // Brave dates the page two ways: `page_age` is an ISO timestamp,
                // `age` a human string ("3 days ago"). Either one lets the agent
                // judge recency against the run's date instead of guessing, and
                // gives `sources_index` a real date to cite.
                let when = "undated";
                if r.page_age != () && r.page_age != "" {
                    when = r.page_age;
                } else if r.age != () && r.age != "" {
                    when = r.age;
                }
                results.push(#{ title: r.title, url: r.url, snippet: r.description, date: when });
            }
        } else {
            engine_problem = "Brave Search answered without a result set (the response carried no `web.results`).";
        }
    } catch(e) {
        engine_problem = `Brave Search failed: ${e}. The key may be invalid or rate limited, or the network is blocked.`;
    }
} else if key_state == "denied" {
    engine_problem = "BRAVE_API_KEY is set but the security layer refused to hand it to this script, because the name looks like a credential. Fix: add `allow_env_vars = [\"BRAVE_API_KEY\"]` under `[security]` in ~/.leviath/config.toml, then restart the daemon.";
} else {
    engine_problem = "BRAVE_API_KEY is not set, so no search engine is configured. Fix: get a key from https://brave.com/search/api/, put it in the daemon's environment, and add `allow_env_vars = [\"BRAVE_API_KEY\"]` under `[security]` in ~/.leviath/config.toml.";
}

let out = "";

if engine_problem == "" {
    if results.len() == 0 {
        // A real search that found nothing is still worth saying out loud - a
        // bare empty array reads as "nothing has been written about this topic".
        out = `[web_search found no results for "${params.query}". The search engine ran and came back empty, so try different wording or a narrower claim. Do not fill the gap from memory.]`;
    } else {
        out = results;
    }
} else {
    // No engine. Wikipedia's search API needs no key and returns clean JSON, so
    // it beats nothing - but it is an encyclopedia, it is undated, and it cannot
    // answer "what is the latest X". Say all of that, every time.
    let wiki = [];
    try {
        let url = `https://en.wikipedia.org/w/rest.php/v1/search/page?q=${encode_uri(params.query)}&limit=${n}`;
        // Wikipedia requires a descriptive User-Agent (403 otherwise).
        let data = parse_json(http_get(url, #{ "User-Agent": "Leviath-Researcher/0.3 (+https://leviath.dev)", "Accept": "application/json" }));
        if data.pages != () {
            for p in data.pages {
                if wiki.len() >= n { break; }
                let snippet = if p.excerpt != () && p.excerpt != "" { p.excerpt } else if p.description != () { p.description } else { "" };
                // Wikipedia wraps matched terms in <span class="searchmatch"> tags - strip them.
                snippet.replace("<span class=\"searchmatch\">", "");
                snippet.replace("</span>", "");
                wiki.push(#{ title: p.title, url: `https://en.wikipedia.org/wiki/${p.key}`, snippet: snippet, date: "undated" });
            }
        }
    } catch(e) {
        // The fallback failed too. The engine problem is the one worth reporting.
    }

    if wiki.len() == 0 {
        out = `[web_search could not search the web. ${engine_problem}
The keyless Wikipedia fallback found nothing for "${params.query}", which is normal - an encyclopedia cannot answer this kind of question.
Treat this topic as UNRESEARCHED. Do not answer it from memory and do not invent sources: report that web search was unavailable, in your findings and in any report you write.]`;
    } else {
        out = `[web_search could not search the web. ${engine_problem}
What follows came from a keyless Wikipedia lookup instead. These are undated encyclopedia articles, NOT web search results, and they are no evidence at all about what is current, recent, or state of the art. Cite them as Wikipedia, and report that web search was unavailable.]
${to_json(wiki)}`;
    }
}

out
