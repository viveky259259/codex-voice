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
| `--cache-ttl 5m\|1h` | `5m` | Lifetime of prompt cache entries. `1h` costs 2x per write (vs 1.25x) and pays off when turns are more than 5 minutes apart. |
| `--no-cache` | | Sends no `cache_control` at all. |
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
| `web_search_tool_type` | `"text"` | Codex's `web_search` tool runs as Anthropic's web search server tool (text results only). |
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
| leading `developer`/`system` messages (base instructions, permissions) | top-level `system` text blocks; the last one carries the stable cache breakpoint |
| later `developer`/`system` messages | user text wrapped in `<developer_message>` (keeps the cached prefix stable) |
| `user` message: `input_text` / `input_image` (data URL or http URL) | `text` / `image` (base64 or url source) |
| `assistant` message `output_text` | assistant `text` |
| `reasoning` with `encrypted_content` written by this proxy | the original `thinking` (text + signature) or `redacted_thinking` block, or a run of server tool blocks (`server_tool_use` + `web_search_tool_result`), verbatim |
| `reasoning` without it (e.g. from an OpenAI model) | dropped |
| `function_call` | `tool_use` (`id` = `call_id`, `input` = the original argument text, byte for byte) |
| `custom_tool_call` | `tool_use` with input `{"input": "<raw text>"}` |
| `local_shell_call` | `tool_use` named `local_shell` with the action as input |
| `function_call_output` / `custom_tool_call_output` | `tool_result` (string, or text/image blocks) |
| `agent_message` | user text `[message from <author>]` |
| `web_search_call` | dropped: display only, the search is replayed from its reasoning item |
| `image_generation_call`, `tool_search_*`, `compaction`, `configuration_update`, others | dropped (debug log) |
| `tools[type=function]` | tool `{name, description, input_schema}` |
| `tools[type=custom]` (freeform, e.g. `apply_patch`) | tool with one string property `input`; the grammar is appended to the description |
| `tools[type=namespace]` | each inner tool flattened to `<namespace>__<name>`; calls come back as `function_call` with `namespace` set |
| `tools[type=web_search]` | server tool `{type: "web_search_20260209" \| "web_search_20250305", name: "web_search"}` (version per model), with `filters.allowed_domains` → `allowed_domains` and `user_location` |
| `tools[type=tool_search \| image_generation \| …]` | dropped |
| tool order | client tools sorted by name, JSON keys sorted, server tool last (stable cached prefix) |
| `tool_choice` `auto` / `none` | `{type: auto}` / `{type: none}` |
| `tool_choice` `required` / a named function | `{type: any}` / `{type: tool}` where the model accepts it and is not thinking; otherwise `{type: auto}` plus a "Respond by calling …" text block in the last user turn |
| `parallel_tool_calls: false` | `disable_parallel_tool_use: true` |
| `reasoning` | `thinking` + `output_config.effort` per model; see [Per-model rules](#per-model-rules) |
| `reasoning.summary` absent or `"none"` | `thinking.display: "omitted"`, otherwise `"summarized"` (Codex omits `summary` when the user chose `none`) |
| `text.format` (json_schema) | `output_config.format` |
| `temperature`, `top_p` | forwarded only to models that accept them (4.6 and older) while thinking is off; dropped otherwise |
| `prompt_cache_options.ttl` (`30m`/`12h`) | 1-hour cache entries (Anthropic's longest) |
| `prompt_cache_key`, `store`, `include`, `service_tier`, `client_metadata`, `text.verbosity` | ignored |

Afterwards the message list is repaired so the Messages API accepts it:
consecutive same-role items are merged, every `tool_use` gets a `tool_result`
at the start of the next user turn (a synthetic `is_error` result if Codex has
none), unmatched results become text, and a `Continue.` user turn is added if
the history starts with, or ends on, an assistant turn.

Tool names that do not match `^[a-zA-Z0-9_-]{1,64}$` are rewritten
deterministically (invalid characters → `_`, truncated, plus an FNV hash);
the proxy keeps a per-request map from Anthropic names back to
`(namespace, name, kind)`.

### Per-model rules

Models are matched by slug (`claude-<family>-<major>-<minor>`, provider
prefixes such as `anthropic.` allowed). Unrecognized slugs get the newest
models' rules, which are the strictest.

| Models | Codex effort `none` | other efforts | forced `tool_choice` | `temperature`/`top_p` | web search tool |
| --- | --- | --- | --- | --- | --- |
| Fable 5.x, Mythos 5.x, Opus 5.5, Sonnet 5.5, unknown | adaptive, `effort: low` | adaptive + effort | → `auto` | dropped | `web_search_20260209` |
| Opus 5, Sonnet 5 | adaptive, `effort: low` | adaptive + effort | → `auto` (thinking is on by default) | dropped | `web_search_20260209` |
| Haiku 5.5 | adaptive, `effort: low` | adaptive + effort | → `auto` (thinking is on by default) | dropped | `web_search_20250305` |
| Opus 4.8 / 4.7 | no `thinking` (off), `effort: low` | adaptive + effort | kept only with thinking off | dropped | `web_search_20260209` |
| Opus 4.6 / Sonnet 4.6 | no `thinking` (off), `effort: low` | adaptive + effort, `xhigh` → `max` | kept only with thinking off | kept only with thinking off | `web_search_20260209` |
| Haiku 4.5, other 4.0–4.5, 3.7 Sonnet | no `thinking` | `budget_tokens`: minimal 1024, low 2048, medium 8192, high 16384, xhigh/max 32000; `max_tokens` ≥ budget + 4096 | kept only with thinking off | kept only with thinking off | `web_search_20250305` |
| Claude 3.5 and older | no `thinking` | no `thinking` | kept | kept | `web_search_20250305` |

- The proxy never sends `thinking: {type: "disabled"}` or `between_tools`:
  they are 400s on Fable 5.x, Opus 5.5 and Sonnet 5.5, allowed only at
  effort ≤ `high` on Opus 5 and Haiku 5.5, and disabling thinking on Opus 5
  has documented failure modes (tool calls written as text). Where omitting
  `thinking` turns it off (Opus 4.8/4.7, the 4.6 family, budget models),
  `none` does that; elsewhere `none` and `minimal` run at `effort: low`.
- `budget_tokens` is only sent to budget-style models.
- A request without `reasoning` uses the model's defaults (no `thinking`, no
  `effort`). Codex always sends `reasoning`.
- Every request ends on a user turn (prefill is a 400 on the 4.6+ family).

### Prompt caching

Codex resends the whole conversation on every request, so the proxy keeps the
rendered prefix (tools → system → messages) byte-stable and marks two of the
four allowed breakpoints:

1. An explicit `cache_control` on the last `system` block (or, without a
   system prompt, the last client tool): tools + system stay cached no matter
   what happens later in the conversation.
2. Top-level `cache_control` (automatic caching): Anthropic places it on the
   last block, so each request writes the prefix the next one reads.

Both use `--cache-ttl` (`{type: "ephemeral"}` or `{…, ttl: "1h"}`). Nothing
volatile is generated into the prefix: no timestamps or request ids, tools are
sorted, schema keys are sorted, and replayed tool calls keep Claude's exact
argument bytes. Later developer messages go into the conversation as
`<developer_message>` user text instead of changing `system`. Prefixes shorter
than the model's minimum (512–4096 tokens) are silently not cached. Watch
`cached_tokens` in Codex's token usage (or `cache_read_input_tokens` in
`--dump-dir` output) to confirm hits.

### Web search

Codex's `web_search` tool becomes Anthropic's web search server tool. Claude
runs the searches inside the same Messages response, so there is no client
loop. Each `server_tool_use` (name `web_search`) is shown to Codex as a
`web_search_call` item (`action: {type: "search", query}`, status `completed`,
or `failed` when the result is an error object). Codex cannot carry Anthropic's
result blocks in `web_search_call` (it strips `encrypted_content` from every
item except `reasoning` for non-OpenAI providers), so each run of server tool
blocks (`server_tool_use`, `web_search_tool_result`, and the code-execution
blocks dynamic filtering may add) is stored verbatim in a `reasoning` item with
an empty summary and `encrypted_content` = `anthropic-server-blocks-v1:` +
base64 JSON. The next request replays those blocks in place in the assistant
turn, so search results stay in Claude's context; the `web_search_call` items
are dropped as display-only. A `server_tool_use` whose result was lost is
dropped on replay.

### Stream (Messages → Responses)

| Messages event | Responses events |
| --- | --- |
| `message_start` | `response.created` (`resp_<message id>`) |
| `text` block | `output_item.added` (message) → `output_text.delta`… → `output_item.done` |
| `thinking` block | `output_item.added` (reasoning) → `reasoning_summary_part.added` → `reasoning_summary_text.delta`… → `.done` → `output_item.done` with `summary` and `encrypted_content` (base64 JSON of the block, prefixed `anthropic-thinking-v1:`) |
| `redacted_thinking` block | reasoning item with the payload in `encrypted_content` |
| `tool_use` block (function tool) | `output_item.added` (function_call) → `function_call_arguments.delta`… → `output_item.done` with the buffered arguments |
| `tool_use` block (custom tool) | `output_item.added` (custom_tool_call) → one `custom_tool_call_input.delta` → `output_item.done` with the raw input |
| `server_tool_use` block (`web_search`) | `output_item.added` (web_search_call, `in_progress`, with the query) |
| `web_search_tool_result` block | `output_item.done` (web_search_call, `completed`/`failed`) |
| run of server tool blocks | `output_item.added` + `.done` (reasoning carrying the blocks), before the next block |
| `citations` / `citations_delta` on text | dropped (the text is kept) |
| `stop_reason` `end_turn` / `tool_use` / `stop_sequence` | `response.completed` |
| `stop_reason` `pause_turn` | `response.completed` with `end_turn: false` |
| `stop_reason` `max_tokens` | `response.incomplete` (`max_output_tokens`) |
| `stop_reason` `refusal` | `response.failed`, code `invalid_prompt` (not retried), message naming the `stop_details` category and explanation |
| `stop_reason` `model_context_window_exceeded` | `response.failed` (`context_length_exceeded`) |
| `error` event | `response.failed` with the Anthropic message; codes `server_is_overloaded`, `rate_limit_exceeded`, `context_length_exceeded`, `invalid_prompt`, … |
| stream ends without `message_stop` | `response.failed` (`server_error`, retried by Codex) |
| usage | `input_tokens` = input + cache read + cache write (Anthropic's `input_tokens` excludes both); `cached_tokens` = `cache_read_input_tokens`; `cache_write_tokens` = `cache_creation_input_tokens` |

Item ids are `msg_/rs_/fc_/ctc_/ws_<message id>_<block index>`; `call_id` is the
Anthropic `tool_use` id, so tool results round-trip.

Non-2xx responses from Anthropic before streaming starts are returned as HTTP
errors in the OpenAI error shape so Codex keeps its retry behaviour: `429` →
`429` (`rate_limit_exceeded`, `retry-after` kept), `529` → `503`
(`server_is_overloaded`), `400`/`413` "prompt is too long" → `400`
(`context_length_exceeded`, which triggers Codex's context-window handling),
other statuses pass through with the Anthropic message.

## Known gaps

- **Hosted tools**: `tool_search`, image generation and other OpenAI-hosted
  tools are dropped. Set `supports_search_tool = false` (as the example
  catalog does) so deferred tools are listed directly. Web search is mapped
  (above), but Anthropic search is always live: Codex's cached mode
  (`external_web_access: false`) has no equivalent and runs live searches.
  `max_uses`, `blocked_domains` and web fetch are not offered.
- **Search citations** on Claude's text are dropped (no `url_citation`
  annotations); the cited results remain in the replayed history.
- **Freeform tools** stream their input in one delta at the end of the block
  (the JSON wrapper is buffered), so `apply_patch` diffs do not render
  incrementally.
- **Replayed `custom_tool_call` input** is re-encoded as `{"input": …}`; JSON
  string escaping may differ from what Claude produced. Function-call
  arguments are replayed byte for byte.
- **Thinking**: only blocks produced by this proxy round-trip. History from an
  OpenAI model (or edited history) loses its reasoning. `reasoning_tokens` is
  always reported as 0 (Anthropic does not split it out).
- **Forced tool choice** on models that reject it is only a prompt
  instruction, so Claude may still answer in text.
- **Model detection** is a slug heuristic; unknown names get the newest
  models' rules. Per-model `max_tokens` limits are not applied (`--max-tokens`
  must fit the smallest model you use, e.g. 8192 for Claude 3.5 Haiku).
- `strict` on function tools, `text.verbosity`, `service_tier`,
  `prompt_cache_key`, refusal fallbacks (`fallbacks`), fast mode, and
  per-request `anthropic-beta` selection are not mapped.
- Image inputs by `file_id`, audio, and file inputs become text placeholders.
- `GET /v1/models` is not implemented; use `model_catalog_json`.
