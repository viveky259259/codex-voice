# Handoff: codex-voice fork (cloud session → local Mac)

## Mac inbox

Requests from the cloud session to the Mac session ("Local: Voice first",
/Users/advait/Documents/Projects/codex-voice). Newest first. To reply, add an
entry under "## Mac outbox" below with the request id, what you ran, and the
results (exact `test result:` lines, errors, log tails), then commit and push to
`wip`. The cloud session polls `wip`.

**Tickets.** Work is now tracked as GitHub issues. The tracker is
https://github.com/viveky259259/codex-voice/issues/25, and each issue has a
status label. Issues labelled `status: ready for Mac test` contain their own
test steps. Report results as a comment on the issue: on success, set
`status: verified on Mac` and close it. The outbox below still works if you
can't comment on issues.

### 2026-11-18 #2: three tickets are ready for Mac test

- #12, the local voice engine: request #1 below has the steps.
- #14, Claude via codex-anthropic-proxy: needs ANTHROPIC_API_KEY.
- #19, `codex remote list/attach` on a real tailnet: needs a second machine.

Do #12 first. #13 (local voice in the TUI) is still being built, and #17, #18,
#20 and #21 wait on it.

### 2026-11-18 #1 (issue #12): standalone local voice (no TUI yet)

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
   command = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-f", "avfoundation",
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
   `uv run mcp_server.py --transport streamable-http`. The tool names and model ids below were confirmed on the Mac (issue #12).

   ```toml
   [capture]
   command = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-f", "avfoundation",
              "-i", ":{device}", "-ac", "1", "-ar", "16000", "-f", "s16le", "-"]
   device = "0"

   [stt]
   backend = "mcp"
   url = "http://127.0.0.1:5051/mcp"
   tool = "submit_inference"
   arguments = { model_id = "hf:mlx-community/whisper-base-mlx", input_path = "{wav}", options = { language = "en" } }

   [stt.job]
   tool = "get_job"
   arguments = { job_id = "{job_id}" }
   poll_interval_ms = 250
   timeout_ms = 120000

   [tts]
   backend = "mcp"
   url = "http://127.0.0.1:5051/mcp"
   tool = "submit_inference"
   arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", prompt = "{text}", options = { voice = "af_heart", language = "en" } }

   [tts.job]
   tool = "get_job"
   arguments = { job_id = "{job_id}" }
   poll_interval_ms = 250
   timeout_ms = 120000

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

## Local voice on the Mac

`[realtime] backend = "local"` makes F8, `/voice` and `auto_start` use the
local voice engine (`codex-rs/local-voice`) instead of OpenAI realtime, so the
`chatgpt.com/backend-api/codex/realtime/calls` 404 no longer matters. The mic
is transcribed locally (ffmpeg + mlx_whisper), each transcript goes through
voice approvals, then voice commands, and otherwise is submitted exactly like
a typed message (it steers a running turn, as Enter does). Prompts, outcomes
and a short summary of each voice turn's answer are spoken with `say`. Typed
turns are not spoken. Code: `tui/src/chatwidget/realtime/local_voice.rs`.
Tested in the cloud with a fake engine only (no audio).

Run every command from the repo root unless a step says otherwise.

### 1. Update and build

```bash
git fetch origin && git checkout wip && git pull --ff-only
cd codex-rs && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p codex-cli -p codex-local-voice -p codex-anthropic-proxy && cd ..
```

The binaries are `codex-rs/target/debug/codex`, `codex-local-voice` and
`codex-anthropic-proxy`.

### 2. Prerequisites

```bash
brew install ffmpeg
uv tool install mlx-whisper          # provides mlx_whisper (pip install mlx-whisper also works)
which ffmpeg mlx_whisper say afplay  # all four must print a path
ffmpeg -hide_banner -f avfoundation -list_devices true -i ""   # note the index of your microphone under "audio devices"
```

These match the engine's macOS defaults (`mlx_whisper ... --model
mlx-community/whisper-base-mlx`, `say`, `afplay`). The first transcription
downloads the whisper model, so it is slow once.

Microphone access: System Settings → Privacy & Security → Microphone → turn on
the terminal app you run Codex in (Terminal, iTerm, Ghostty...). Quit and
reopen that terminal after changing it. If access is denied, macOS often
gives ffmpeg silence instead of an error: the footer then stays on
"listening" and never shows "hearing you".

### 3. Check the engine on its own

```bash
cat > /tmp/voice-say.toml <<'EOF'
[capture]
device = "0"

[stt]
backend = "command"
command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-base-mlx",
           "--output-format", "txt", "--output-dir", "{outdir}", "--verbose", "False"]
timeout_ms = 60000

[tts]
backend = "command"
command = ["say", "{text}"]
EOF
codex-rs/target/debug/codex-local-voice config | head -20
codex-rs/target/debug/codex-local-voice say --config /tmp/voice-say.toml "Hello from Codex"
codex-rs/target/debug/codex-local-voice listen --config /tmp/voice-say.toml
```

Change `device = "0"` if your microphone has another index. `say` must be
heard and end with `SpeakingFinished`. In `listen`, say "what time is it": you
should see `SpeechStarted`, `SpeechEnded`, then a line like `Transcript {
text: "What time is it?" }`. Type `hello` and Enter: it is spoken and NOT transcribed back.
Ctrl-C quits. Fix this step before going on; the TUI uses the same engine.

### 4. A text model that works without the ChatGPT model gate

The fork reports version 0.0.0, which is the likely reason the ChatGPT account
rejects the configured model. Pick ONE option. Each command writes the top of
`~/.codex/config.toml` (your old file is kept as `config.toml.bak`); step 5
appends the voice part.

**(a) Fully local with Ollama.** Needs Ollama 0.13.4 or newer (Codex talks to
its Responses API and refuses older versions with `--oss`), and about 16 GB of
free memory for the default `gpt-oss:20b`.

```bash
brew install ollama
brew services start ollama
ollama --version
ollama pull gpt-oss:20b
mkdir -p ~/.codex && cp ~/.codex/config.toml ~/.codex/config.toml.bak 2>/dev/null
cat > ~/.codex/config.toml <<EOF
log_dir = "$HOME/.codex/log"
approval_policy = "untrusted"
model_provider = "ollama"
model = "gpt-oss:20b"
EOF
```

Without the config you can also run `codex-rs/target/debug/codex --oss
--local-provider ollama -m gpt-oss:20b` (it checks the Ollama version and pulls
the model if missing).

**(b) Claude through codex-anthropic-proxy.** In a second terminal, from the
repo root:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
codex-rs/target/debug/codex-anthropic-proxy --port 18765
```

Then, in the first terminal, from the repo root:

```bash
mkdir -p ~/.codex && cp ~/.codex/config.toml ~/.codex/config.toml.bak 2>/dev/null
cat > ~/.codex/config.toml <<EOF
log_dir = "$HOME/.codex/log"
approval_policy = "untrusted"
model = "claude-opus-5-5"
model_provider = "anthropic-proxy"
model_catalog_json = "$PWD/codex-rs/anthropic-proxy/model_catalog.example.json"

[model_providers.anthropic-proxy]
name = "Anthropic (via codex-anthropic-proxy)"
base_url = "http://127.0.0.1:18765/v1"
wire_api = "responses"
requires_openai_auth = false
stream_idle_timeout_ms = 600000
EOF
```

**(c) An OpenAI API key.** Replace `gpt-5.1` with a model your key can use.

```bash
export OPENAI_API_KEY=sk-...
mkdir -p ~/.codex && cp ~/.codex/config.toml ~/.codex/config.toml.bak 2>/dev/null
cat > ~/.codex/config.toml <<EOF
log_dir = "$HOME/.codex/log"
approval_policy = "untrusted"
model = "gpt-5.1"
model_provider = "openai-apikey"

[model_providers.openai-apikey]
name = "OpenAI (API key)"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
requires_openai_auth = false
EOF
```

`approval_policy = "untrusted"` makes Codex ask before running commands, so
step 6 gets an approval prompt to answer by voice. `log_dir` turns on the log
file used in step 7.

### 5. Turn on local voice

```bash
cat >> ~/.codex/config.toml <<'EOF'

[realtime]
backend = "local"
auto_start = true
voice_commands = true

[realtime.local.capture]
device = "0"

[realtime.local.stt]
backend = "command"
command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-base-mlx",
           "--output-format", "txt", "--output-dir", "{outdir}", "--verbose", "False"]
timeout_ms = 60000

[realtime.local.tts]
backend = "command"
command = ["say", "{text}"]
EOF
cat ~/.codex/config.toml
```

Use the same `device` as in step 3. `[realtime] type = "conversational"` is
not needed for the local backend. `[realtime.local]` takes the same tables as
the `--config` files of `codex-local-voice` (`capture`, `vad`, `stt`, `tts`,
`playback`, `transcripts`); leave it out entirely to use the macOS defaults.

Alternative, the local-agents MCP server for both directions: start it
(`uv run mcp_server.py --transport streamable-http`), check its tools with
`codex-rs/target/debug/codex-local-voice tools http://127.0.0.1:5051/mcp`
(expect `submit_inference` and `get_job`), and replace the
`[realtime.local.stt]` and `[realtime.local.tts]` blocks above with these
(the names and model ids from the Mac test in #12, as in
`codex-rs/local-voice/README.md`):

```toml
[realtime.local.stt]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "submit_inference"
arguments = { model_id = "hf:mlx-community/whisper-base-mlx", input_path = "{wav}", options = { language = "en" } }

[realtime.local.stt.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }
poll_interval_ms = 250
timeout_ms = 120000

[realtime.local.tts]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "submit_inference"
arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", prompt = "{text}", options = { voice = "af_heart", language = "en" } }

[realtime.local.tts.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }
poll_interval_ms = 250
timeout_ms = 120000

[realtime.local.playback]
player = ["afplay", "{wav}"]
```

### 6. Test it in the TUI

```bash
mkdir -p /tmp/voice-test && cd /tmp/voice-test && git init -q
RUST_LOG=codex_tui=debug,codex_local_voice=debug ~/Documents/Projects/codex-voice/codex-rs/target/debug/codex
```

(Adjust the path if the repo is elsewhere. Option (a) without config: add
`--oss --local-provider ollama -m gpt-oss:20b`.)

1. Answer the folder trust prompt with the keyboard if it appears. Then you
   see "Voice conversation started (local speech)." and the
   footer shows `voice ● listening`. If not, press F8.
2. Say "what files are in this folder". The footer shows `hearing you`, then
   `transcribing`; your words appear as a user message and a turn starts.
   When it ends you hear a one or two sentence answer (code is never read
   out; long answers end with "Details are on screen.").
3. Say "create a file called hello.txt containing hi". When the approval
   prompt opens you hear "Codex wants to … Say approve or deny."
4. Say "approve". You hear "Approve … ? Say confirm." Nothing runs yet.
   Say "confirm". The command runs and the turn continues.
5. Ask for something else that runs a command ("delete hello.txt"). At the
   prompt say "no". You hear "Denied." and nothing is sent to the model.
6. Say "what's running". You hear the agent list (a voice command; no turn
   starts).
7. Press F9. The footer shows `muted`; speech in progress stops; talking does
   nothing. Press F9 again: `listening`.
8. Type a question and press Enter. The answer is shown but not spoken.
9. Press F8. You see "Voice conversation ended." and the footer strip goes
   away. `pgrep -fl "ffmpeg|mlx_whisper|afplay"` in another terminal prints
   nothing. Quitting Codex (`/quit`) also stops them.

Keys: F8 or `/voice` toggles, F9 or `/voice mute` mutes (and cuts off speech).
Esc is not bound to "stop speaking": it already primes backtrack when the
composer is empty and interrupts a running turn. While an approval prompt is
open, speech that is not an answer or a voice command is not sent; Codex says
"Say approve or deny."

### 7. What to report

In the Mac outbox or on the issue:
- which model option (a/b/c) and whether turns worked at all;
- for each of steps 6.1–6.9: pass, or what you saw and heard instead;
- any error message shown in the TUI, word for word (engine errors appear
  once, with a fix);
- the log: `tail -n 200 ~/.codex/log/codex-tui.log` (and
  `grep -i "local voice\|codex_local_voice" ~/.codex/log/codex-tui.log | tail -n 50`).
  The log only exists with `log_dir` set (step 4); `RUST_LOG` as in step 6.

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
