# cc-proxy

Anthropic Messages API → DeepSeek Chat Completions API proxy. Converts Claude Code's traffic into DeepSeek-compatible requests with full thinking/reasoning/KV-cache support.

## Architecture

```
Claude Code                          DeepSeek API
(Anthropic /v1/messages)  ──►  (OpenAI /v1/chat/completions)
        │                                  │
        └──── cc-proxy ─────────────┘
             127.0.0.1:11435
```

## Features

- **Anthropic → OpenAI format conversion**: Full `/v1/messages` → `/v1/chat/completions` translation
- **Thinking/reasoning support**: `budget_tokens` → `reasoning_effort` (`max`/`high`) + `thinking: {type: "enabled"}`
- **Reasoning content replay**: Deterministic 3-state reasoning replay (same → replay, tool_calls → placeholder, text-only → omit) — preserves DeepSeek prefix-cache byte stability
- **Streaming SSE**: Proper `Thinking → Text → ToolUse` block transitions in streaming responses
- **Sanitization**: Auto-inject `"(reasoning omitted)"` placeholder for tool-call messages missing reasoning (prevents DeepSeek 400)
- **Orphan tool_call cleanup**: Full tool_call/tool_result ID-set matching
- **Tool result dedup + compression**: SHA-256 content fingerprinting + HEAD 4000 / TAIL 4000 truncation

### Design principles (from CodeWhale & Reasonix)

> Reasoning replay must be a function of the stored message ONLY, never of later history. DeepSeek's prefix cache hashes the raw bytes of every message; flipping `reasoning_content` on/off depending on whether a follow-up user turn exists rewrites a historical message between turns and busts the cache from that point onwards.

## Quick Start

```bash
# Build
cargo build --release

# Run
LISTEN_ADDR=127.0.0.1:11435 \
DEEPSEEK_BASE_URL=http://127.0.0.1:11434/v1 \
DEEPSEEK_API_KEY=not-needed \
RUST_LOG=info \
./target/release/cc-proxy
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_ADDR` | `0.0.0.0:11435` | Listen address |
| `DEEPSEEK_BASE_URL` | `http://127.0.0.1:11434/v1` | Upstream DeepSeek-compatible API |
| `DEEPSEEK_API_KEY` | `not-needed` | API key for upstream |
| `RUST_LOG` | `info` | Log level |

### Claude Code Configuration

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:11435",
    "ANTHROPIC_AUTH_TOKEN": "not-needed"
  }
}
```

## Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/messages` | POST | Anthropic Messages API (with streaming via `stream: true`) |
| `/v1/models` | GET | Model list |
| `/health` | GET | Health check; also reports the effective config summary and model-resolution counters |

## Model Mapping

Production routing lives in `~/.config/cc-proxy/config.toml` (selected by
`MODEL_CONFIG_PATH`). The table below is only the built-in fallback used when
no config file is present.

| Claude Code Model | Built-in default upstream |
|-------------------|---------------------------|
| `claude-opus-4-7` | `deepseek-v4-pro` |
| `claude-sonnet-4-6` | `deepseek-v4-flash` |
| `claude-haiku-4-5` | `deepseek-v4-flash` |

### Resolution order

1. profile / alias pass-through, after stripping the `[1m]` context marker;
2. exact `[models.mapping]` key;
3. mapping key after stripping a structural suffix (`-YYYYMMDD`, `-latest`,
   Bedrock-style `-vN:M`), so dated ids such as
   `claude-sonnet-4-5-20250929` resolve instead of silently degrading;
4. `[models].default`, counted as a fallback and warned once per inbound name.

`GET /health` reports the effective config plus `model_resolution` counters
(`profile_hits`, `mapping_hits`, `normalized_hits`, `fallbacks`) and the
offending inbound names. Startup also fails fast when a mapping target does not
resolve to a model profile.

## Thinking Mode → Reasoning Effort

### Chat wire (`/v1/chat/completions`)

| Input | `reasoning_effort` |
|-------|--------------------|
| `output_config.effort` present | resolved through the provider's `effort_map` |
| absent | legacy `xhigh` → `max` |

### Responses wire (`/v1/responses`)

Effort precedence: `output_config.effort` > thinking budget > legacy `max`.

| Thinking mode | `reasoning.effort` |
|---|---|
| `budget_tokens` < 4096 | `low` |
| 4096–9999 | `medium` |
| 10000–19999 | `high` |
| ≥ 20000 / `max` | `max` |
| `adaptive` / budget 0 | `high` |
| `disabled` | provider's lowest supported level, **no summary** |
| no `thinking` block | field omitted (legacy shape) |

GPT-family note: the gateway accepts `low / medium / high / xhigh / max` for
`gpt-6.1-sol`; **`none` and `minimal` are rejected by some backends (HTTP 400)**,
so `disabled` resolves to `low` instead of `none`. `xhigh` is aliased to `max`
in `[providers.gpt.effort_map]`: Claude Code's top effort level means "the
highest tier", which upstream is `max` (`ultra` is not a valid enum value).

## Credits

Inspired by:
- [CodeWhale](https://github.com/Hmbown/CodeWhale) — DeepSeek agentic coding client
- [Reasonix](https://github.com/esengine/deepseek-reasonix) — DeepSeek-first terminal AI agent
