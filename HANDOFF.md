# Handoff: codex-voice fork (cloud session → local Mac)

## Mac inbox

Requests from the cloud session to the Mac session ("Local: Voice first",
/Users/advait/Documents/Projects/codex-voice). Newest first. To reply, add an
entry under "## Mac outbox" below with the request id, what you ran, and the
results (exact `test result:` lines, errors, log tails), then commit and push to
`wip`. The cloud session polls `wip`.

### 2026-11-18 #1: standalone local voice (no TUI yet)

Status: the TUI wiring of local voice (`[realtime] backend = "local"`) is being
built in the cloud now. Until it lands, `/voice` still uses OpenAI realtime and
404s. That is expected. The next inbox entry will say when it lands and how to
test it. This request tests the audio engine on its own with its CLI.

1. Update and build (from the repo root):

   ```bash
   git fetch origin && git checkout wip && git pull --ff-only
   cd codex-rs && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p codex-local-voice
   ```

2. Prerequisites:

   ```bash
   brew install ffmpeg
   uv tool install mlx-whisper        # provides the mlx_whisper command
   ffmpeg -hide_banner -f avfoundation -list_devices true -i ""   # note the audio input index
   ```

   The app that runs the commands (Claude.app or your terminal) needs microphone
   access: System Settings → Privacy & Security → Microphone. The first
   `listen` run may trigger the prompt.

3. Save as `codex-rs/voice-say.toml` (ffmpeg avfoundation capture + `say` +
   mlx_whisper). Set `device` to the index from step 2:

   ```toml
   [capture]
   command = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "avfoundation",
              "-i", ":{device}", "-ac", "1", "-ar", "16000", "-f", "s16le", "-"]
   device = "0"

   [stt]
   backend = "command"
   command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-base-mlx",
              "--output-format", "txt", "--output-dir", "{outdir}", "--verbose", "False"]
   timeout_ms = 60000

   [tts]
   backend = "command"
   command = ["say", "{text}"]
   ```

4. Save as `codex-rs/voice-mcp.toml` (local-agents MCP at
   http://127.0.0.1:5051/mcp for both directions). Start the server first with
   `uv run mcp_server.py --transport streamable-http`. The tool and argument
   names below are guesses, because `mcp_server.py` isn't pushed to
   viveky259259/local-agents. Fix them from the output of
   `cargo run -p codex-local-voice -- tools http://127.0.0.1:5051/mcp`.

   ```toml
   [capture]
   command = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "avfoundation",
              "-i", ":{device}", "-ac", "1", "-ar", "16000", "-f", "s16le", "-"]
   device = "0"

   [stt]
   backend = "mcp"
   url = "http://127.0.0.1:5051/mcp"
   tool = "transcribe"
   arguments = { model_id = "whisper:base", input_path = "{wav}" }
   # text_path = "result.text"

   [stt.job]
   tool = "get_job"
   arguments = { job_id = "{job_id}" }
   poll_interval_ms = 250
   timeout_ms = 120000

   [tts]
   backend = "mcp"
   url = "http://127.0.0.1:5051/mcp"
   tool = "speak"
   arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", voice = "af_heart", text = "{text}" }
   # audio_path = "artifacts.0.path"
   # artifact_dir = "/path/to/local-agents/artifacts"

   [tts.job]
   tool = "get_job"
   arguments = { job_id = "{job_id}" }

   [playback]
   player = ["afplay", "{wav}"]
   ```

5. Run, from `codex-rs/`:

   ```bash
   cargo test -p codex-local-voice
   cargo run -p codex-local-voice -- config
   cargo run -p codex-local-voice -- say --config voice-say.toml "Hello from Codex"
   RUST_LOG=codex_local_voice=debug cargo run -p codex-local-voice -- listen --config voice-say.toml
   ```

   In `listen`, ask the user to say two sentences with a pause between them.
   Each should print one `Transcript` event. Type a line to hear it spoken,
   and check that the spoken audio is NOT transcribed back (half duplex).
   `/stop` stops speech. Ctrl-C quits. Then repeat `say` and `listen` with
   `--config voice-mcp.toml` (after `tools`).

6. Report in the outbox:
   - the `test result:` line
   - whether `say` was heard
   - the transcripts printed vs. what was said, and the latency
   - any self-transcription
   - the `tools` output (tool names and argument schemas)
   - any changes you had to make to the two TOML files
   - error messages

## Mac outbox

(Mac session: add replies here, newest first.)


Goal: a voice-first fork of OpenAI Codex that also runs Claude models, attaches to
Codex on other machines over Tailscale/SSH (from T3 Code), and orchestrates
running agents (from Ambient-style workflows).

## Working model
Development continues in the cloud session; the Mac is the test bench
(live voice, and anything too big for the cloud container's disk). Pull
`wip`, run the tests below, and report results back to the cloud session.

## Branches (github.com/viveky259259/codex-voice)
- `main` — upstream openai/codex at 8011b92, squashed (Apache-2.0).
- `codex-voice` — tested work: Lane B (Claude proxy) and Lane A (voice TUI).
- `wip` — `codex-voice` + Lane C (not yet compiled in full) + this file.
  Test from `wip`.

## Also on codex-voice
- `codex-rs/voice-commands` (`codex-voice-commands`): pure parser for spoken
  orchestration / model / machine commands ("what's my desktop agent doing",
  "tell worker two to run the tests", "switch to claude", "connect to my mac
  studio"). 47 tests pass. Wired into the TUI on `wip` (with
  `codex-orchestrator`): see "Voice commands" in CODEX-VOICE.md. Run
  `cargo test -p codex-tui --lib -- voice_command`. Not yet tried live.

## Voice orchestration across machines (wip)
`[realtime] voice_machines = ["devbox", "me@mac-studio"]` makes the TUI's voice
orchestrator drive agents on those machines over SSH (Lane C's `ssh://`
endpoint), next to this machine's. `AppServerAgentBackend` is generic over an
`AppServerTransport` (`tui/src/app/voice_command_backend.rs`); the SSH link
(`voice_command_remote.rs`) connects in the background when voice starts,
reports "connecting" until ready, keeps the connection, reconnects after a
failure, and kills the ssh child on drop. Tested: codex-config parsing,
orchestrator with fake connecting/dead remotes, the mapping over a fake
transport, and the real link through a fake `ssh` to a fake app server
(`cargo test -p codex-tui --lib -- voice_command`). Not yet: a real remote
host, a live voice session.

Mac checks: add a reachable host to `voice_machines`, start voice, say "what's
running", "list my machines", then steer and stop an agent there.

## Remote approvals by voice (wip)
Approval requests from agents on `voice_machines` hosts open in this TUI's
approval overlay, labeled "devbox · build", and are answered by keyboard or
voice ("Build on devbox wants to run cargo test. Say approve or deny." →
"approve" → "Approve cargo test for build on devbox? Say confirm." →
"confirm"). Code: `tui/src/app/voice_remote_approvals.rs` (registry keyed by
(machine, request id), alias thread ids, answer routing in
`submit_thread_op`), `voice_command_remote.rs` (subscribe with
`thread/resume { excludeTurns }`, forward requests / `serverRequest/resolved`
/ `turn/completed` / disconnect). Tested in the cloud: conversion, routing,
App flow over fake WebSocket servers, widget voice flow, and one request over
the fake `ssh` (`cargo test -p codex-tui --lib -- voice_remote_approvals
remote_approval remote_denial answered_elsewhere`); the voice suite
(`realtime approval voice`) passes, 422 tests. Not yet: a real remote host,
live voice. Side effects of the
subscribe (from reading app-server code): the remote thread records this
connection's client name and stays loaded while subscribed.

Mac checks: with a `voice_machines` host, start an agent there that needs an
approval (e.g. `approval_policy = "untrusted"`), start voice here, say
"what's running", wait for "… on devbox wants to …", say "approve", then
"confirm"; also answer one at the remote and check the prompt here closes.

## On wip only (untested)
- `035f3f0`-equivalent fix commit "Voice approvals: apply each spoken answer
  once" (review findings 1–4). Run the codex-tui realtime/approval/voice tests.
- Lane C (below).

## Mac test checklist (always `export RUST_MIN_STACK=8388608`, as upstream CI)
Also `export CARGO_NET_GIT_FETCH_WITH_CLI=true` on a first build: libgit2 failed
to clone the v8 Chromium submodule (abseil-cpp) on the Mac; the git CLI works.

### Mac results so far (wip 1b18208, macOS arm64)
- Lane A: codex-tui realtime/approval/voice 366 passed. codex-core lib 731 passed;
  core tests/all.rs 22 failures are unrelated to the fork (helper bins not prebuilt,
  code_mode). That run predates the review fix: rerun on current wip.
- Lane C: all green after two test-setup fixes (now committed): tailnet 19,
  app-server-client 69 + 1 + ssh_remote 3, cli remote_cmd 4, cli remote_ssh 3.
  Lane C is now on codex-voice.
- Not yet: live voice, real-tailnet remote list/attach, Claude via the real API.

1. Lane C compile + tests (never compiled in the cloud):
   `cargo test -p codex-cli --test remote_ssh`, `cargo test -p codex-cli remote_cmd`,
   `cargo test -p codex-tailnet`, `cargo test -p codex-app-server-client`.
2. Lane A full check: `cargo test -p codex-tui --lib -- realtime approval voice`
   (the 2 cloud failures were disk-full SQLite errors).
3. Live voice: GStreamer >= 1.28, `[realtime] auto_start = true`, `codex`, speak.
4. `codex remote list` on a machine with Tailscale; `codex remote attach <host>`.
5. Claude: run `codex-anthropic-proxy` with `ANTHROPIC_API_KEY`, then `codex` with
   the Lane B config below.

## Lane status
| Lane | Where | Status |
|---|---|---|
| D: Voice orchestration across machines | `codex-rs/orchestrator/`, `tui/src/app/voice_command*.rs`, `tui/src/app/voice_remote_approvals*.rs`, `config/src/config_toml.rs` (`voice_machines`) | On `wip`. Includes remote approvals by voice. Fake-ssh integration tests pass in the cloud; real remote host not yet tried. |
| B: Claude models | `codex-rs/anthropic-proxy/` | Committed. 27 tests pass vs a mock Anthropic server. Not run against the real API. Drafted-but-not-added test that feeds proxy output through `codex_api::spawn_response_stream` (needs dev-deps codex-api, codex-client, codex-protocol, futures). README has config + mapping table. |
| A: Voice-first TUI | `codex-rs/tui/` (+ `config/src/config_toml.rs`, `core/src/config/*`, `core/config.schema.json`) | Committed. 362 realtime/approval/voice TUI tests pass incl. 30 new (2 failed only on disk-full); codex-config 418 pass. The stack overflows were from not setting `RUST_MIN_STACK=8388608` (upstream always does). `cargo test -p codex-core config` not yet run. |
| C: Tailscale/SSH remote | `codex-rs/tailnet/`, `codex-rs/app-server-client/` (`ssh.rs`, `remote*.rs`, `tests/ssh_remote.rs`), `codex-rs/cli/` (`remote_cmd.rs`, `remote_cmd_tests.rs`, `tests/remote_ssh.rs`, `main.rs`, `Cargo.toml`), small edits in `tui/src/app/exit_summary.rs`, `tui/src/status/remote_connection.rs` | On `wip` only. Passed: codex-tailnet tests; app-server-client lib 69; `ssh_remote` integration 3 (fake `ssh` → real local app-server). NOT compiled: the `cli/` parts (`codex remote list` / `attach`). Adds `ssh://host` remote endpoint (runs `ssh host -- codex app-server proxy`). |

## Lane A details
Config:
```toml
[realtime]
type = "conversational"
auto_start = true
```
Voice approval grammar (pure module `tui/src/chatwidget/realtime/voice_approval.rs`):
approve/yes/allow/go ahead → asks "Approve <summary>? Say confirm." → only
"confirm" applies; deny/no/reject → immediate; stop/cancel/abort → abort;
any negation word (not, don't, never, wait, if, maybe, but) blocks a match.
Known gap: if the realtime model hands an approval phrase off anyway, core
has already appended it to the turn; the TUI hides it but blocking it needs a
core change.

Live voice needs the packaged `codex-voice-host` + GStreamer >= 1.28 (voice-host
crate). That could not run in the cloud container (Ubuntu 24.04 has 1.24) —
this is the main reason to continue on a Mac. Voice backend is OpenAI realtime
(`gpt-live-1.1-codex`, `openai-alpha: quicksilver=v2` header) — unverified
whether a normal API key / ChatGPT login is accepted. Check first.

## Lane B config
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
Run the proxy with `ANTHROPIC_API_KEY` set (see its README for flags).

## Remaining plan
1. Get Lane A green (stack-size rerun), commit; get Lane C green, commit.
   Commit each lane only with its own files (shared `codex-rs/Cargo.toml` /
   `Cargo.lock` carry both `anthropic-proxy` and `tailnet` members).
2. Real-API smoke tests: Claude via the proxy; live voice on the Mac.
3. Wave 2: Lane D — multi-machine agents overview + voice commands
   ("what's my desktop agent doing", "tell it to run the tests") on top of
   app-server `thread/list`, `thread/read`, `turn/steer`, `turn/interrupt`
   and `codex agents` (`tui/src/app/agents_overview*.rs`). Lane E — voice
   control of model switching / remote attach. Lane F — remote + voice.
4. Hardening: wider test pass, code review, README/demo script.

## Build notes
- Test per crate: `cargo test -p <crate>`; never the whole workspace unless needed.
- In the cloud we linked through a debug-section-compressing wrapper only to
  save disk; not needed on a Mac.
- Read `AGENTS.md` for repo conventions (rustfmt edition 2024, pretty_assertions).
