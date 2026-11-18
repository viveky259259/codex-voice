# codex-voice

A fork of [OpenAI Codex](https://github.com/openai/codex) (Apache-2.0) that you
drive by voice, that runs Claude models as well as OpenAI models, that attaches
to Codex on your other machines over Tailscale/SSH, and that lets one voice
session see and steer agents running elsewhere.

Upstream base: `openai/codex@8011b92`, imported as the first commit on `main`.

| Feature | Where | Status |
|---|---|---|
| Voice-first TUI: auto-start, spoken approvals with confirmation, spoken status | `codex-rs/tui` | Tested (no-audio harness). Live audio: verify on a Mac. |
| Claude models via a local translator | `codex-rs/anthropic-proxy` | Tested against a mock Anthropic server. Real API: not yet. |
| Voice commands: "what's my desktop agent doing", "switch to Claude"… | `codex-rs/voice-commands` | Parser tested. Not wired into the TUI yet. |
| Orchestration engine: list, read, steer and stop agents across machines | `codex-rs/orchestrator` | Tested with a fake backend. App-server backend and TUI wiring: next, on the Mac. |
| `codex remote list` / `codex remote attach` (Tailscale + SSH) | `codex-rs/tailnet`, `app-server-client`, `cli` | Tested on macOS with fake ssh/tailscale against a real local server. Real tailnet: not yet. |

Branches: `codex-voice` holds tested work; `wip` adds work still being tested
(see `HANDOFF.md` there).

## Build

```bash
cd codex-rs
cargo build -p codex-cli -p codex-anthropic-proxy
```

Tests follow upstream CI and need a bigger test stack:

```bash
RUST_MIN_STACK=8388608 cargo test -p codex-tui --lib -- realtime approval voice
```

## Voice

Live voice needs Codex's packaged voice helper (`codex-voice-host`) and
GStreamer 1.28 or newer, which in practice means macOS, glibc Linux or Windows
with GStreamer installed. Voice uses OpenAI's realtime service.

`~/.codex/config.toml`:

```toml
[realtime]
type = "conversational"
auto_start = true   # start listening as soon as a session opens
```

F8 or `/voice` still toggles voice by hand; F9 mutes.

Spoken approvals, while an approval prompt is open:

| Say | Effect |
|---|---|
| "approve", "yes", "allow", "go ahead" | Codex asks "Approve <command>? Say confirm." |
| "confirm" | Applies the approval you just armed, for that prompt only. |
| "approve for this session", "always allow" | Same, with session scope (if offered). |
| "no", "deny", "reject" | Denies immediately. |
| "stop", "cancel", "abort" | Aborts immediately. |

Anything with a negation or hedge ("don't approve", "maybe") is never an
answer. Only your own words count: if the voice model relays "confirm" on your
behalf, nothing is approved.

## Claude models

Start the translator with your key, then point Codex at it.

```bash
ANTHROPIC_API_KEY=... cargo run -p codex-anthropic-proxy -- --port 18765
```

```toml
model = "claude-opus-5-5"
model_provider = "anthropic-proxy"
model_catalog_json = "/ABS/PATH/codex-rs/anthropic-proxy/model_catalog.example.json"

[model_providers.anthropic-proxy]
name = "Anthropic (via codex-anthropic-proxy)"
base_url = "http://127.0.0.1:18765/v1"
wire_api = "responses"
requires_openai_auth = false
```

The catalog tells Codex about Claude's context window and enables the
apply_patch tool. See `codex-rs/anthropic-proxy/README.md` for the full mapping
and known gaps (OpenAI-hosted tools such as web search are dropped).

## Other machines

```bash
codex remote list                 # Tailscale peers + ~/.ssh/config hosts
codex remote attach devbox        # TUI on this machine, Codex on devbox
codex --remote ssh://me@devbox    # same, without the helper
```

The remote machine needs Codex installed and SSH access. SSH is the only
authentication; no tokens are sent. Discovery is ported from T3 Code (MIT).

## Demo script

1. Start the Claude translator; launch `codex` with the config above.
2. Codex opens and starts listening. Say: "add a test for the parser and run it".
3. When the shell approval appears, Codex says "Codex wants to run cargo
   test…". Say "approve", then "confirm".
4. Hear the spoken summary of the result.
5. Say "no" to the next prompt and hear "Denied."
6. (When wired) "what's running", then "what's the desktop agent doing", then
   "tell it to run the tests", then "confirm".

## Credits

- OpenAI Codex, Apache-2.0: the base of this fork.
- T3 Code (pingdotgg/t3code), MIT: Tailscale/SSH discovery approach.
