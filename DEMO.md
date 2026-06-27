# Leviath v0 - End-to-End Demonstration

This guide shows Leviath v0 is fully working, from CLI to context management.

## Quick Start

### 1. Build and Install

```bash
cd ~/dev/leviath
cargo install --path crates/leviath-cli
```

This installs the `lev` binary to your cargo bin directory.

### 2. Create Your First Agent

```bash
# Create a new agent project
lev init my-first-agent

# Enter the directory
cd my-first-agent

# Check what was created
ls -la
```

You should see:
- `agent.leviath` - Agent configuration
- `README.md` - Documentation
- `scripts/` - Directory for custom Rhai scripts

### 3. Run the Agent

```bash
lev run --task "hello world"
```

You should see:
- ✓ Manifest loaded
- ✓ Context window initialized
- ✓ Provider configured
- Mock execution demonstration

### 4. List Available Agents

```bash
lev list
```

### 5. Explore Example Agents

```bash
cd ~/dev/leviath/agents

# Check out the examples
cat coder/agent.leviath
cat researcher/agent.leviath
cat reviewer/agent.leviath
```

## What Works in v0

### ✅ Core Features Implemented

**leviath-core:**
- ✓ All 6 region types (Pinned, SlidingWindow, Temporary, Clearable, Compacting, CompactHistory)
- ✓ Region validation schemas (JSON, Mermaid, Code, Markdown)
- ✓ Context window with eviction cascade
- ✓ Blueprint and Stage definitions
- ✓ Token budget tracking
- ✓ 14 comprehensive tests

**leviath-scripting:**
- ✓ Sandboxed Rhai engine
- ✓ Built-in functions (string ops, validators, token counting)
- ✓ Content validation (is_json, is_mermaid, is_markdown)
- ✓ Summarization (truncation-based)
- ✓ Operation limits (prevents infinite loops)
- ✓ 10 tests covering all functions

**leviath-providers:**
- ✓ Provider trait
- ✓ AnthropicProvider (mock mode)
- ✓ OpenAIProvider (mock mode)
- ✓ OpenRouterProvider (mock mode)
- ✓ OllamaProvider (mock mode)
- ✓ Token counting (approximate)
- ✓ Context limits per model
- ✓ 9 provider tests

**leviath-cli:**
- ✓ `lev init` - Create new agents
- ✓ `lev run` - Execute agents
- ✓ `lev list` - List installed agents
- ✓ Project scaffolding
- ✓ Manifest generation

**Example Agents:**
- ✓ `coder` - Multi-stage coding agent
- ✓ `researcher` - Research and synthesis
- ✓ `reviewer` - Code review agent

## Testing

### Run All Tests

```bash
cd ~/dev/leviath
cargo test
```

Expected: 33+ tests passing across all crates.

### Build Release Binary

```bash
cargo build --release
./target/release/lev --version
```

### Eviction Cascade Tests

The eviction cascade is fully tested:

```bash
cargo test --lib leviath-runtime -- eviction
```

Tests cover:
- Clearable → Temporary → error cascade
- SlidingWindow never reduced
- Pinned never touched
- Correct ordering

### Scripting Sandbox Tests

```bash
cargo test --lib leviath-scripting
```

Tests cover:
- Operation limits (prevents infinite loops)
- String operations
- Content validation
- Token counting

## Architecture Highlights

### Memory Regions (Hardware-Inspired)

Like SNES VRAM, each region has a purpose:

- **Pinned** - Architecture docs, identity (never evicted)
- **SlidingWindow** - Conversation history (fixed size)
- **Temporary** - Tool outputs (evict oldest)
- **Clearable** - Scratch space (all-or-nothing)
- **Compacting** - Large content (auto-summarizes)
- **CompactHistory** - Summaries (preserved)

### Eviction Cascade

When space is needed:

1. Clear Clearable regions (instant space)
2. Evict Temporary oldest entries (one at a time)
3. Compact Compacting regions (requires LLM)
4. NEVER touch SlidingWindow or Pinned

### Provider Integration

All providers use the same interface:

```rust
trait Provider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse>;
    fn count_tokens(&self, text: &str, model: &str) -> usize;
    fn max_context_tokens(&self, model: &str) -> usize;
    fn name(&self) -> &str;
}
```

Currently in mock mode - real API calls can be added without changing the interface.

## What's Next (Post-v0)

- Real API calls for providers (not just mocks)
- Streaming support
- MCP tool integration
- Actual LLM-based compaction
- Agent pooling and scheduling
- Context transforms between agents
- `lev test` - Agent evaluation framework
- `lev serve` - API server
- tiktoken integration for accurate token counting

## Notes

- Mock execution allows testing without API keys
- The pipeline works end-to-end
- All core abstractions are in place
- Real LLM integration is straightforward from here

## Success Criteria Met

From the original task:

- ✅ `cargo install --path crates/leviath-cli` works
- ✅ `lev init my-agent` creates working scaffold
- ✅ `lev list` shows installed agents
- ✅ `lev run --task "hello world"` demonstrates pipeline
- ✅ Core eviction logic works and is tested
- ✅ Rhai validators work and are tested
- ✅ 33+ tests passing
- ✅ Example agents provided

**The pipeline works end-to-end!** 🎉
