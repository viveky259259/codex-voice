# codex-anthropic-proxy

Runs Anthropic Claude models inside Codex without changing Codex itself.

Codex's model client only speaks the OpenAI **Responses API**. This crate is a
small localhost server that accepts `POST /v1/responses` (streaming SSE) from
Codex, translates the request into an Anthropic **Messages API** request
(`POST {ANTHROPIC_BASE}/v1/messages`, `stream: true`, headers `x-api-key` and
`anthropic-version: 2023-06-01`), and translates the Messages event stream back
into Responses events that Codex's parser accepts.

```
codex ──Responses SSE──▶ codex-anthropic-proxy (127.0.0.1) ──Messages SSE──▶ api.anthropic.com
```

## Running it

```sh
export ANTHROPIC_API_KEY=sk-ant-...      # read from the environment, never logged
cargo run -p codex-anthropic-proxy -- --port 18765
```

Flags:

| Flag | Default | Purpose |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Listen address. Keep it on loopback: the proxy attaches your key. |
| `--port` | `18765` | Listen port (`0` = ephemeral). |
| `--server-info FILE` | | Writes `{"port":…,"pid":…}` once listening. |
| `--anthropic-base-url URL` | `https://api.anthropic.com` | Upstream base; `/v1/messages` is appended. |
| `--anthropic-version V` | `2023-06-01` | `anthropic-version` header. |
| `--anthropic-beta LIST` | | Optional `anthropic-beta` header. |
| `--max-tokens N` | `64000` | `max_tokens` per request (raised automatically to fit a legacy thinking budget). |
| `--dump-dir DIR` | | Records every exchange (Responses request, translated Anthropic request, raw Anthropic SSE, emitted Responses SSE). Headers are never recorded. Useful for writing fixtures. |

`RUST_LOG=codex_anthropic_proxy=debug` logs every dropped item, tool, or event.
`GET /health` returns `ok`.

## Pointing Codex at it

`~/.codex/config.toml`:

```toml
model = "claude-opus-5-5"
model_provider = "anthropic-proxy"
# Without a catalog Codex falls back to generic metadata for unknown slugs:
# no apply_patch tool and no reasoning-effort levels.
model_catalog_json = "/absolute/path/to/codex-rs/anthropic-proxy/model_catalog.example.json"

[model_providers.anthropic-proxy]
name = "Anthropic (via codex-anthropic-proxy)"
base_url = "http://127.0.0.1:18765/v1"
wire_api = "responses"
requires_openai_auth = false
# Optional: let Codex wait longer on long thinking turns.
stream_idle_timeout_ms = 600000
```

Custom providers use HTTP SSE (not websockets), so each request carries the full
stateless history; `previous_response_id` is never needed.

### Model catalog

`model_catalog_json` replaces Codex's whole model list (so the picker shows only
these models) and is parsed as `ModelsResponse` from
`protocol/src/openai_models.rs`. The shipped
[`model_catalog.example.json`](model_catalog.example.json) lists
`claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` and
`claude-fable-5-2`. Fields that matter:

| Field | Value | Why |
| --- | --- | --- |
| `slug` | Anthropic model id | Sent unchanged as `model`. |
| `base_instructions` (or `model_messages.instructions_template`) | Codex's generic `models-manager/prompt.md` | **Required**: the loader rejects a model without instructions. |
| `apply_patch_tool_type` | `"freeform"` | Codex sends `apply_patch` as a `custom` tool (the fallback metadata sends none). |
| `shell_type` | `"shell_command"` | Same shell tool as current OpenAI models. |
| `supported_reasoning_levels` / `default_reasoning_level` | `low`…`max` | Mapped to Claude `output_config.effort`. |
| `default_reasoning_summary` | `"auto"` | Thinking summaries stream into the reasoning UI (`"none"` hides them). |
| `supports_search_tool` | `false` | Keeps Codex from sending the hosted `tool_search` tool, which has no Claude equivalent. |
| `web_search_tool_type` | `"text"` | Hosted web search is dropped by the proxy anyway. |
| `context_window` / `max_context_window` | `1000000` | Claude 5.x context; drives auto-compaction. |
| `use_responses_lite` | `false` | Tools are sent in `tools`, not as input items. |
| `support_verbosity` | `false` | No Anthropic equivalent. |

Other required keys (`display_name`, `description`, `visibility`,
`supported_in_api`, `priority`, `availability_nux`, `upgrade`,
`default_verbosity`, `truncation_policy`, `experimental_supported_tools`) are
present with neutral values.

## Translation

### Request (Responses → Messages)

| Responses | Messages |
| --- | --- |
| `model` | `model` (unchanged) |
| leading `developer`/`system` messages (base instructions, permissions) | top-level `system` text blocks |
| later `developer`/`system` messages | user text wrapped in `<developer_message>` (keeps the cached prefix stable) |
| `user` message: `input_text` / `input_image` (data URL or http URL) | `text` / `image` (base64 or url source) |
| `assistant` message `output_text` | assistant `text` |
| `reasoning` with `encrypted_content` written by this proxy | the original `thinking` (text + signature) or `redacted_thinking` block, verbatim |
| `reasoning` without it (e.g. from an OpenAI model) | dropped |
| `function_call` | `tool_use` (`id` = `call_id`, `input` = the original argument text, byte for byte) |
| `custom_tool_call` | `tool_use` with input `{"input": "<raw text>"}` |
| `local_shell_call` | `tool_use` named `local_shell` with the action as input |
| `function_call_output` / `custom_tool_call_output` | `tool_result` (string, or text/image blocks) |
| `agent_message` | user text `[message from <author>]` |
| `web_search_call`, `image_generation_call`, `tool_search_*`, `compaction`, `configuration_update`, others | dropped (debug log) |
| `tools[type=function]` | tool `{name, description, input_schema}` |
| `tools[type=custom]` (freeform, e.g. `apply_patch`) | tool with one string property `input`; the grammar is appended to the description |
| `tools[type=namespace]` | each inner tool flattened to `<namespace>__<name>`; calls come back as `function_call` with `namespace` set |
| `tools[type=web_search \| tool_search \| …]` | dropped |
| `tool_choice` `auto` / `none` / `required` | `{type: auto}` / `{type: none}` / `{type: any}` |
| `parallel_tool_calls: false` | `disable_parallel_tool_use: true` |
| `reasoning.effort` (Claude 4.6+) | `thinking: {type: "adaptive"}` + `output_config.effort` (`none`/`minimal` → `low`; `xhigh` → `max` on 4.6) |
| `reasoning.effort` (Claude 3.7 – 4.5) | `thinking: {type: "enabled", budget_tokens}`: minimal 1024, low 2048, medium 8192, high 16384, xhigh 32000; `max_tokens` ≥ budget + 4096 |
| `reasoning.summary: "none"` | `thinking.display: "omitted"`, otherwise `"summarized"` |
| `text.format` (json_schema) | `output_config.format` |
| `prompt_cache_key`, `store`, `include`, `service_tier`, `client_metadata`, `text.verbosity` | ignored; top-level `cache_control: {type: "ephemeral"}` enables automatic prompt caching |

Afterwards the message list is repaired so the Messages API accepts it:
consecutive same-role items are merged, every `tool_use` gets a `tool_result`
at the start of the next user turn (a synthetic `is_error` result if Codex has
none), unmatched results become text, and a `Continue.` user turn is added if
the history starts with, or ends on, an assistant turn.

Tool names that do not match `^[a-zA-Z0-9_-]{1,64}$` are rewritten
deterministically (invalid characters → `_`, truncated, plus an FNV hash);
the proxy keeps a per-request map from Anthropic names back to
`(namespace, name, kind)`.

### Stream (Messages → Responses)

| Messages event | Responses events |
| --- | --- |
| `message_start` | `response.created` (`resp_<message id>`) |
| `text` block | `output_item.added` (message) → `output_text.delta`… → `output_item.done` |
| `thinking` block | `output_item.added` (reasoning) → `reasoning_summary_part.added` → `reasoning_summary_text.delta`… → `.done` → `output_item.done` with `summary` and `encrypted_content` (base64 JSON of the block, prefixed `anthropic-thinking-v1:`) |
| `redacted_thinking` block | reasoning item with the payload in `encrypted_content` |
| `tool_use` block (function tool) | `output_item.added` (function_call) → `function_call_arguments.delta`… → `output_item.done` with the buffered arguments |
| `tool_use` block (custom tool) | `output_item.added` (custom_tool_call) → one `custom_tool_call_input.delta` → `output_item.done` with the raw input |
| `stop_reason` `end_turn` / `tool_use` / `stop_sequence` | `response.completed` |
| `stop_reason` `pause_turn` | `response.completed` with `end_turn: false` |
| `stop_reason` `max_tokens` | `response.incomplete` (`max_output_tokens`) |
| `stop_reason` `refusal` | `response.incomplete` (`content_filter`) |
| `stop_reason` `model_context_window_exceeded` | `response.failed` (`context_length_exceeded`) |
| `error` event | `response.failed` with the Anthropic message; codes `server_is_overloaded`, `rate_limit_exceeded`, `context_length_exceeded`, `invalid_prompt`, … |
| stream ends without `message_stop` | `response.failed` (`server_error`, retried by Codex) |
| usage | `input_tokens` = input + cache read + cache write; `cached_tokens` = `cache_read_input_tokens`; `cache_write_tokens` = `cache_creation_input_tokens` |

Item ids are `msg_/rs_/fc_/ctc_<message id>_<block index>`; `call_id` is the
Anthropic `tool_use` id, so tool results round-trip.

Non-2xx responses from Anthropic before streaming starts are returned as HTTP
errors in the OpenAI error shape so Codex keeps its retry behaviour: `429` →
`429` (`rate_limit_exceeded`, `retry-after` kept), `529` → `503`
(`server_is_overloaded`), `400`/`413` "prompt is too long" → `400`
(`context_length_exceeded`, which triggers Codex's context-window handling),
other statuses pass through with the Anthropic message.

## Known gaps

- **Hosted tools**: `web_search`, `tool_search`, image generation and other
  OpenAI-hosted tools are dropped; Anthropic server tools are not offered.
  Set `supports_search_tool = false` (as the example catalog does) so deferred
  tools are listed directly.
- **Freeform tools** stream their input in one delta at the end of the block
  (the JSON wrapper is buffered), so `apply_patch` diffs do not render
  incrementally.
- **Replayed `custom_tool_call` input** is re-encoded as `{"input": …}`; JSON
  string escaping may differ from what Claude produced. Function-call
  arguments are replayed byte for byte.
- **Thinking**: only blocks produced by this proxy round-trip. History from an
  OpenAI model (or edited history) loses its reasoning. Claude 5.x cannot
  disable thinking, so Codex `none`/`minimal` effort maps to `low`.
  `reasoning_tokens` is always reported as 0 (Anthropic does not split it out).
- **Model detection** for thinking style is a slug heuristic (`claude-<family>-<major>-<minor>`);
  unknown names are treated as current (adaptive) models.
- `strict` on function tools, `text.verbosity`, `service_tier`,
  `prompt_cache_key`, refusal fallbacks (`fallbacks`), fast mode, and
  per-request `anthropic-beta` selection are not mapped.
- Image inputs by `file_id`, audio, and file inputs become text placeholders.
- `GET /v1/models` is not implemented; use `model_catalog_json`.
