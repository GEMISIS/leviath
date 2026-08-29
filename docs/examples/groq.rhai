// @provider groq
// @description Groq inference (OpenAI-compatible, fast)
// @supports_streaming true
// @default_model llama-3.3-70b-versatile
// @max_context_tokens 131072
// @max_output_tokens 32768
//
// Drop this into ~/.leviath/providers/groq.rhai and set GROQ_API_KEY, then point
// an agent stage at `provider = "groq"`. See docs/content/rhai-providers.md.

fn initialize(config) {
    let api_key = config.api_key ?? env_var("GROQ_API_KEY");
    if api_key == () { throw #{ message: "GROQ_API_KEY not set", transient: false }; }
    #{
        base_url: config.base_url ?? "https://api.groq.com/openai/v1",
        api_key: api_key,
        model: config.model ?? "llama-3.3-70b-versatile",
    }
}

fn auth_headers(state) {
    #{
        "Authorization": `Bearer ${state.api_key}`,
        "Content-Type": "application/json",
    }
}

fn build_messages(request) {
    let msgs = [];
    if request.system.len() > 0 {
        let sys_text = request.system.map(|b| b.text).reduce(|a, b| a + "\n\n" + b, "");
        msgs.push(#{ role: "system", content: sys_text });
    }
    for msg in request.messages {
        msgs.push(#{ role: msg.role, content: msg.content });
    }
    msgs
}

fn build_tools(request) {
    request.tools.map(|t| #{
        type: "function",
        function: #{ name: t.name, description: t.description, parameters: t.parameters },
    })
}

fn build_body(state, request, streaming) {
    let body = #{
        model: request.model ?? state.model,
        messages: build_messages(request),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
    };
    let tools = build_tools(request);
    if tools.len() > 0 { body.tools = tools; }
    if streaming { body.stream = true; }
    to_json(body)
}

fn map_finish(reason) {
    switch reason {
        "stop" => "Complete",
        "tool_calls" => "ToolCall",
        "length" => "TokenLimit",
        _ => "Complete",
    }
}

fn usage_of(u) {
    if u == () { return #{ total_tokens: 0 }; }
    #{
        prompt_tokens: u.prompt_tokens ?? 0,
        completion_tokens: u.completion_tokens ?? 0,
        total_tokens: u.total_tokens ?? 0,
        cached_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn inference(state, request) {
    let resp = parse_json(http_post(
        `${state.base_url}/chat/completions`,
        build_body(state, request, false),
        auth_headers(state),
    ));
    let choice = resp.choices[0];
    let msg = choice.message;
    let tool_calls = if msg.tool_calls != () {
        msg.tool_calls.map(|tc| #{
            id: tc.id,
            name: tc.function.name,
            arguments: parse_json(tc.function.arguments),
        })
    } else { [] };
    #{
        content: msg.content ?? "",
        tool_calls: tool_calls,
        tokens_used: usage_of(resp.usage),
        finish_reason: map_finish(choice.finish_reason),
    }
}

fn stream(state, request, on_chunk) {
    stream_request(
        `${state.base_url}/chat/completions`,
        build_body(state, request, true),
        auth_headers(state),
        |chunk| {
            let data = parse_sse(chunk);
            if data == () { return; }
            let choice = data.choices[0];
            let delta = choice.delta;
            let result = #{ delta: delta.content ?? "" };
            if choice.finish_reason != () {
                result.finish_reason = map_finish(choice.finish_reason);
            }
            if data.usage != () { result.tokens = usage_of(data.usage); }
            on_chunk.call(result);
        },
    );
}

// Called by the context-window guard before a request large enough to be
// near the model's window goes out (small turns never reach it). Groq has no
// counting endpoint, so this answers with the local estimate. An API that has
// one is asked instead, with the estimate as the fallback, so a failed count
// never turns into a failed request:
//
//     fn count_tokens(state, text, model) {
//         try {
//             let resp = parse_json(http_post(
//                 `${state.base_url}/count_tokens`,
//                 to_json(#{ model: model, messages: [#{ role: "user", content: text }] }),
//                 auth_headers(state),
//             ));
//             resp.input_tokens
//         } catch {
//             count_tokens_heuristic(text, "openai")
//         }
//     }
fn count_tokens(state, text, model) {
    count_tokens_heuristic(text, "openai")
}

fn list_models(state) {
    let resp = parse_json(http_get(`${state.base_url}/models`, auth_headers(state)));
    resp.data.map(|m| #{
        id: m.id,
        display_name: m.id,
        max_context_tokens: m.context_window ?? 8192,
        max_output_tokens: 4096,
    })
}
