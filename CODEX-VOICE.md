# codex-voice

A fork of [OpenAI Codex](https://github.com/openai/codex) (Apache-2.0) that you
drive by voice, that runs Claude models as well as OpenAI models, that attaches
to Codex on your other machines over Tailscale/SSH, and that lets one voice
session see and steer agents running elsewhere.

Upstream base: `openai/codex@8011b92`, imported as the first commit on `main`.

| Feature | Where | Status |
|---|---|---|
| Voice-first TUI: auto-start, spoken approvals with confirmation, spoken status | `codex-rs/tui` | Tested (no-audio harness). Live audio: verify on a Mac. |
| Local voice (recommended): local speech-to-text and text-to-speech, no OpenAI realtime service | `codex-rs/local-voice`, `codex-rs/tui/src/chatwidget/realtime/local_voice.rs` | Engine tested with fake audio commands; TUI wiring tested with a fake engine. Live audio: verify on a Mac (`HANDOFF.md`, "Local voice on the Mac"). |
| Claude models via a local translator | `codex-rs/anthropic-proxy` | Tested against a mock Anthropic server. Real API: not yet. |
| Claude Code agent sessions (`codex --agent claude-code`), the T3 Code way | `codex-rs/claude-agent-backend`, `codex-rs/tui/src/agent_backend.rs` | Tested against a fake `claude` CLI (stream-json + SDK control protocol). Real `claude` login: verify on a Mac (`HANDOFF.md`, inbox #4). |
| Voice commands: "what's my desktop agent doing", "switch to Claude"… | `codex-rs/voice-commands` | Parser tested. Wired into the TUI. |
| Orchestration engine: list, read, steer and stop agents across machines | `codex-rs/orchestrator`, `codex-rs/tui/src/app/voice_command*.rs` | Wired into the TUI for this machine's agents and for the machines in `[realtime] voice_machines` (over SSH). Tested against fake app servers and a fake `ssh`; not yet with a real remote host or a live voice session. |
| Approvals from agents on other machines, by voice or keyboard | `codex-rs/tui/src/app/voice_remote_approvals.rs`, `voice_command_remote.rs` | Tested against fake app servers and a fake `ssh`; not yet with a real remote host. |
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

## Install on a Mac

```bash
scripts/codex-voice/install.sh
```

This builds `codex` and `codex-local-voice` in release mode (about 20 minutes
the first time; `--skip-build` installs an existing `codex-rs/target/release`
build). It installs them, the voice helpers and a config into
`~/.codex-voice`, which keeps its settings and sessions apart from
`~/.codex`. The `codex-voice` launcher goes into `~/.local/bin`. Existing
`config.toml` and `voice.conf` files are kept. You need ffmpeg, uv and a
signed-in Claude Code, and the terminal needs microphone access.

```bash
codex-voice              # Claude Code session, listening right away
codex-voice -m sonnet    # pick the model
codex-voice models       # voice models and voices on this Mac
```

The launcher keeps a Whisper server loaded for the session, which makes
transcripts 3-4x faster. It stops the server when you quit.

### Choosing voice models

Pick the speech models in `~/.codex-voice/voice.conf`:

| Setting | Values |
|---|---|
| `tts` | `say` (default) or `local-models` |
| `say_voice` | a macOS voice (`say -v '?'`); empty for the system voice |
| `tts_model`, `tts_voice` | a speech model and voice from the local-models gateway, e.g. `hf:mlx-community/Kokoro-82M-8bit` and `af_heart` |
| `stt` | `whisper-server` (default), `mlx_whisper` or `local-models` |
| `stt_model` | the Whisper model, e.g. `mlx-community/whisper-small-mlx` |
| `language` | `en` |
| `mcp_url` | the gateway, `http://127.0.0.1:5051/mcp` |

`local-models` is the MCP gateway in local-agents (`uv run mcp_server.py
--transport streamable-http`), which serves Kokoro, Chatterbox and other
models from this Mac's caches. Each request runs as a separate job. Kokoro
takes about 7 s per sentence once warm and up to 90 s for the first one, so
`say` is better for quick back-and-forth. Only cached voices work, because the
gateway runs offline. If the gateway is down, the launcher falls back to `say`
or mlx_whisper and says so.

Environment variables override `voice.conf` for one run. They are the setting
names, upper-cased, with a `CODEX_VOICE_` prefix:

```bash
CODEX_VOICE_TTS=local-models CODEX_VOICE_TTS_VOICE=hf_alpha codex-voice
```

## Voice

Local voice is the recommended voice path. It needs no OpenAI realtime
service and no voice host: ffmpeg captures the microphone, a local
speech-to-text command (mlx_whisper on macOS) transcribes each utterance, and
`say` speaks. Each transcript is handled as a spoken approval answer, then as
a voice command, and otherwise submitted exactly like a typed message (it
steers a running turn the way Enter does). Prompts and outcomes are spoken,
and so is a short summary of each answer to a spoken request (at most two
sentences, no code; "Details are on screen." when shortened). Answers to
typed requests are not spoken. Any text model works, including Ollama and
Claude through the proxy below.

`~/.codex/config.toml`:

```toml
[realtime]
backend = "local"       # default "openai": OpenAI realtime (upstream behavior)
auto_start = true       # start listening as soon as a session opens
voice_commands = true   # the default; false sends every utterance to the model

# Optional: everything below has macOS defaults (ffmpeg device 0,
# mlx_whisper whisper-large-v3-turbo, say, afplay). Same tables as codex-local-voice's
# config: capture, vad, stt, tts, playback, transcripts.
[realtime.local.capture]
device = "0"            # ffmpeg -f avfoundation -list_devices true -i ""

[realtime.local.stt]
backend = "command"
command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-large-v3-turbo", "--language", "en",
           "--output-format", "txt", "--output-dir", "{outdir}", "--verbose", "False"]

[realtime.local.tts]
backend = "command"
command = ["say", "{text}"]
```

Install `brew install ffmpeg` and `uv tool install mlx-whisper`, and give the
terminal microphone access (System Settings → Privacy & Security →
Microphone). A missing program or a denied microphone is reported once, with
the fix. See `codex-rs/local-voice/README.md` for other speech-to-text and
text-to-speech backends (whisper.cpp server, Kokoro, an MCP server), and
`HANDOFF.md` ("Local voice on the Mac") for a step-by-step test.

F8 or `/voice` toggles voice by hand; F9 mutes the microphone and cuts off
speech in progress. While an approval prompt is open, speech that is neither
an answer nor a voice command is not sent ("Say approve or deny.").

With `backend = "openai"`, live voice needs Codex's packaged voice helper
(`codex-voice-host`), GStreamer 1.28 or newer and `type = "conversational"`,
and uses OpenAI's realtime service. With a ChatGPT login this fork's realtime
call currently fails (404), so prefer the local backend.

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
behalf, nothing is approved. (Local voice has no voice model: only your
transcript is ever read.)

Voice commands, during a voice conversation (approvals come first while a
prompt is open):

| Say | Effect |
|---|---|
| "what's running", "list my agents" | Lists loaded agents on this machine and every configured machine, and names machines that are still connecting or didn't answer. |
| "what's build doing", "check on build" | Reads that agent's status and latest step. |
| "tell build to run the tests" | Asks "Tell build on <host>: run the tests. Say confirm."; only "confirm" sends it. |
| "stop build" | Asks "Stop build on <host>? Say confirm."; only "confirm" interrupts it. |
| "switch to opus" | Applies the model like the `/model` picker. |
| "list my machines" | Lists this machine, configured machines (connected, connecting, unreachable), and Tailscale peers / `~/.ssh/config` hosts (not connected). |
| "connect to studio" | Says "Run codex remote attach studio." (no runtime attach yet). |

Anything else after "Say confirm." cancels. When two agents share a name,
Codex asks which one. "It" means the last agent you asked about. Everything
that is not a command goes to the model as before.

### Voice commands across machines

List the machines whose agents voice commands may drive. Only these are ever
contacted; machines found by `codex remote list` are names only.

```toml
[realtime]
voice_machines = [
  "devbox",                       # any destination `codex remote attach` accepts
  "me@mac-studio",
  { host = "ssh://me@10.0.0.7:2222", name = "nas", codex_bin = "/opt/codex/bin/codex" },
]
```

Each machine is reached the way `codex remote attach` reaches it: `ssh HOST --
codex app-server daemon start`, then `ssh HOST -- codex app-server proxy`, with
`BatchMode=yes` (keys, an agent or Tailscale SSH; no prompts). `codex_bin` (or
`ssh://HOST?codex_bin=PATH`, or `CODEX_REMOTE_BIN`) points at Codex when it is
not on the remote non-interactive `PATH`. Machines are named in speech by
`name`, else the host without user, port or domain ("mac studio").

- Connections start in the background when voice starts and are kept. Until a
  machine's first connection is up (a command waits at most 1.5 s), it is
  reported as connecting: "Two agents. Build on desktop: running cargo test,
  2 minutes. Tests on devbox: idle. Mac studio is still connecting, nas
  didn't answer."
- A dropped or failed connection is retried on the next command; a dead
  machine only fails its own commands ("I can't reach nas.").
- "tell build to run the tests" asks "Tell build on devbox: run the tests. Say
  confirm." Steer and stop on another machine always wait for "confirm".
- When the voice orchestrator stops, its SSH children are killed.

### Approvals from other machines

When an agent on a configured machine asks for approval, the prompt opens in
this TUI, in the same approval overlay as local prompts, labeled with machine
and agent ("devbox · build"). Answer it with the keyboard as usual, or by voice:

| You hear / say | Effect |
|---|---|
| "Build on devbox wants to run cargo test. Say approve or deny." | The prompt is open. |
| "approve" | "Approve cargo test for build on devbox? Say confirm." |
| "confirm" | The approval goes back to devbox, over its SSH connection. |
| "deny", "no" | Denied at once, on devbox. |

- Each machine's connection listens to the agents it lists (when it connects
  and on every "what's running") with `thread/resume`, without history or
  setting overrides; requests already waiting there are shown when it
  subscribes. Agents started on that machine later are picked up on the next
  "what's running". This machine's agents are not subscribed twice.
- Answered at the machine itself (or by another client), or its turn ended:
  the prompt here closes, and a half-said "approve" is dropped.
- Connection lost: its prompts close and Codex says "Lost devbox; its approval
  is still waiting there." If an answer cannot be sent: "Couldn't reach
  devbox; the approval is still waiting there." Nothing is ever answered
  automatically; the request keeps waiting on that machine.
- Only machines in `voice_machines` are listened to. A remote file edit
  prompt shows no diff (it stays on that machine) and is spoken as "edit
  files".

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

## Claude Code sessions

Run the session on Claude Code instead of Codex. The `claude` CLI does the
work with its own tools, permissions and login (no API key); the TUI, its
approval overlay, voice approvals and voice commands work unchanged.

```bash
claude                                   # once: log in with /login
codex --agent claude-code                # Claude Code's default model
codex --agent claude-code -m opus        # or any id `claude` accepts
codex --agent claude-code --claude-bin /opt/homebrew/bin/claude
codex --no-daemon --agent claude-code    # a dev build (cargo build -p codex-cli)
```

A dev build has no complete local package, so it needs `--no-daemon`.

- The header and `/status` say "Claude Code". `-m` (or `model` in config.toml)
  is passed as `--model` only when it names a Claude model.
- `/model` lists the models the CLI reports, plus claude-fable-5-2,
  claude-opus-5-5, claude-sonnet-5-5 and claude-haiku-5-5. Picking one, or
  saying "switch to opus" / "sonnet" / "haiku" / "fable", switches the running
  session (`set_model`) without restarting it. It is not saved as Codex's
  default model. The effort level picked after a model applies to the running
  session too (`apply_flag_settings`). `resume` continues with the model and
  effort the session last used.
- Voice answers are spoken, as in Codex sessions. `/status` shows
  "Permissions: Claude Code (asks before risky tools)": Codex's sandbox does
  not apply.
- Claude Code's permission prompts appear as Codex approvals: shell commands
  as command approvals ("Claude Code wants to run …" when spoken), edits with
  their diff, other tools as permission prompts, questions as a question form.
  Approve for this session adds a session rule in Claude Code; Cancel also
  stops the turn.
- One backend per process: a Claude Code session cannot be combined with
  `--remote`, and Codex and Claude threads do not mix yet. Images, steering a
  running turn and history replay on resume are not supported yet. See
  `codex-rs/claude-agent-backend/README.md` for the wire format and mapping.

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
- T3 Code (pingdotgg/t3code), MIT: Tailscale/SSH discovery approach, and the
  Claude Code adapter design (`ClaudeAdapterV2`, `ClaudeModelCatalog`).
