# Context Window Refactor — Structured Messages

**Goal:** Messages region holds the chronological conversation (agent ↔ LLM) with typed entries. Everything else goes in system-bound regions. Provider-specific serialization handles the API format differences.

**Baseline:** 98K lines, 3,531 tests, all passing.

## Principles

1. **Messages = "what happened between agent and LLM"** — user turns, assistant turns (with tool_use), tool results. All in one region, in order.
2. **Everything else = system context** — task specs, compacted history, scratch. Goes into the system prompt (or equivalent).
3. **No text-prefix role detection** — entries carry typed metadata, not "Assistant: " string prefixes.
4. **Provider serialization is a separate concern** — the internal model is provider-agnostic; each provider knows how to format it for its API.
5. **Simplify where possible** — remove complexity that existed to work around the broken model.

## Phase 1: Core Types (leviath-core, leviath-providers)

### 1a. `EntryKind` on `RegionEntry` (leviath-core/region.rs)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryKind {
    /// Plain text (system content, summaries, scratch)
    Text,
    /// User message in conversation
    UserMessage,
    /// Assistant response with optional tool calls
    AssistantTurn {
        tool_calls: Vec<SerializedToolCall>,
    },
    /// Tool execution result, paired with a tool_call_id
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
```

Add `pub kind: EntryKind` to `RegionEntry` (default `EntryKind::Text` for backward compat).

### 1b. Rich `Message` type (leviath-providers/provider.rs)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}
```

Update `Message.content` from `String` to `MessageContent`. Add `impl From<String>` for easy migration.

### 1c. `InferenceRequest` gets a `system` field

```rust
pub struct InferenceRequest {
    pub system: Vec<SystemBlock>,  // NEW: system prompt sections
    pub messages: Vec<Message>,     // Now: only conversation messages
    pub model: String,
    pub max_tokens: usize,
    pub temperature: f32,
    pub tools: Vec<Tool>,
    pub extra: serde_json::Value,
}

pub struct SystemBlock {
    pub text: String,
    pub cache_hint: CacheHint,
}
```

This separates system context from conversation messages at the type level.

## Phase 2: Context Assembly (leviath-runtime)

### 2a. Replace `assemble_messages()` with `assemble()`

New method on `ContextWindow`:

```rust
pub struct AssembledContext {
    pub system_blocks: Vec<SystemBlock>,
    pub messages: Vec<Message>,
}

impl ContextWindow {
    pub fn assemble(&self) -> AssembledContext {
        // System-bound regions (Pinned, CompactHistory) → system_blocks
        // Messages region (SlidingWindow) → messages with proper types
        // Other regions (Clearable, Temporary, Compacting) → system_blocks
    }
}
```

- Pinned regions → system blocks with `CacheHint::Always`
- CompactHistory → system blocks
- SlidingWindow (messages) → `Vec<Message>` built from typed `RegionEntry`s
- Compacting/Clearable/Temporary → system blocks (these hold non-conversation context)
- If no user message exists in the messages region, inject `"Begin."` as the first message

Delete `assemble_messages()` entirely.

### 2b. Update the inference loop (engine.rs)

**Storing assistant responses:**
```rust
// OLD:
window.add_to_region("conversation", format!("Assistant: {}", response.content), tokens);

// NEW:
window.add_typed_entry("messages", EntryKind::AssistantTurn {
    tool_calls: response.tool_calls.iter().map(|tc| SerializedToolCall {
        id: tc.id.clone(),
        name: tc.name.clone(),
        arguments: tc.arguments.clone(),
    }).collect(),
}, response.content, tokens);
```

**Storing tool results:**
```rust
// OLD: routed to different regions based on tool_result_routing
window.add_to_region(actual_region, format!("[Tool {}]: {}", id, result), tokens);

// NEW: always in messages region
window.add_typed_entry("messages", EntryKind::ToolResult {
    tool_call_id: id.clone(),
    tool_name: name.to_string(),
    is_error: false,
}, result_text, tokens);
```

**Storing user messages / nudges:**
```rust
// OLD:
window.add_to_region("conversation", format!("User: {}", nudge), tokens);

// NEW:
window.add_typed_entry("messages", EntryKind::UserMessage, nudge.to_string(), tokens);
```

### 2c. Remove `tool_result_routing`

This config currently controls which region tool results go to. Since all tool results now go in messages, remove:
- `ToolResultRoutingConfig` struct
- `tool_result_routing` field from `Stage`
- All routing logic in the inference loop
- Related blueprint parsing

If Gerald wants compaction priority hints later (e.g., "summarize bash output more aggressively"), that can be a future feature on the compaction system, not the routing system.

## Phase 3: Provider Serialization (leviath-providers)

### 3a. Anthropic serialization (anthropic.rs)

`build_request_body` takes `InferenceRequest` (which now has `system` + typed `messages`) and produces:

```json
{
    "system": [{"type": "text", "text": "...", "cache_control": ...}],
    "messages": [
        {"role": "user", "content": "Begin."},
        {"role": "assistant", "content": [
            {"type": "text", "text": "I'll start by reading..."},
            {"type": "tool_use", "id": "toolu_01...", "name": "list_dir", "input": {"path": "."}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01...", "content": "file1.py\nfile2.py"}
        ]}
    ]
}
```

### 3b. OpenAI serialization (openai_compat.rs)

```json
{
    "messages": [
        {"role": "system", "content": "..."},
        {"role": "user", "content": "Begin."},
        {"role": "assistant", "content": "I'll start by...", "tool_calls": [
            {"id": "call_01", "type": "function", "function": {"name": "list_dir", "arguments": "{...}"}}
        ]},
        {"role": "tool", "tool_call_id": "call_01", "content": "file1.py\nfile2.py"}
    ]
}
```

### 3c. Other providers (gemini, ollama, openrouter)

All use OpenAI-compatible format via `build_openai_request_body`. Update once, all benefit.

## Phase 4: Cleanup & Simplification

### Remove:
- `tool_result_routing` config and all routing logic
- Text-prefix role detection in `assemble_messages()` (entire function)
- The `format!("Assistant: {}", ...)` / `format!("User: {}", ...)` pattern everywhere
- `format!("[Tool {}]: {}", ...)` pattern
- Separate "tool_results" region from default context layouts (agents can still define one for other purposes, but tools don't auto-route there)

### Simplify:
- Region kinds: evaluate if `Temporary` is still needed or if it's just `Clearable` with different eviction
- Edge transforms: work on typed entries, not text parsing
- Compaction: compact turn groups (assistant + tool results = one unit), produce text summaries for the history region
- Dashboard context view: render typed entries with proper formatting

### Blueprint validation:
- Fail if no SlidingWindow region exists (agent creator must define messages region)
- Warn if a region named "messages" or "conversation" isn't SlidingWindow
- Validate that `entry_stage` region layout includes a messages-capable region

## Phase 5: Tests

- All existing 3,531 tests must pass (or be updated to match new types)
- New tests for:
  - Typed entry serialization round-trip
  - Anthropic message format validation (alternating roles, tool_use/tool_result pairing)
  - OpenAI message format validation
  - Compaction of turn groups
  - Blueprint validation rejection without messages region
  - Edge transform with typed entries
  - Mid-run message delivery with typed entries

## Migration

### Blueprint changes:
- `tool_result_routing` sections become no-ops (warn + ignore) then remove in next minor version
- Default context layout should include a `messages` region (SlidingWindow)

### Internal:
- `RegionEntry` gets `kind: EntryKind` with `#[serde(default)]` defaulting to `Text` for old serialized data
- `Message.content` uses `MessageContent` with `From<String>` for gradual migration

## Order of Work

1. Core types (EntryKind, MessageContent, SystemBlock) — no behavioral changes
2. Provider serialization — make providers handle new types
3. Context assembly — new `assemble()` method alongside old `assemble_messages()`
4. Inference loop — switch to typed entries
5. Remove old code — delete `assemble_messages()`, text prefixes, routing
6. Blueprint validation — add messages region check
7. Edge transforms & compaction — update for typed entries
8. Dashboard — render typed entries
9. Test sweep — update all affected tests
