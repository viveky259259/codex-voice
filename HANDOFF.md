# Handoff: codex-voice fork (cloud session → local Mac)

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
  studio"). 47 tests pass. NOT yet wired into the TUI. Wiring is Mac work
  (needs a codex-tui build); the exact plan is in its README, "TUI integration".

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
