# codex-orchestrator

The engine behind voice orchestration: from one voice session, list the
agents (Codex threads) running on every machine, read what one is doing,
steer it ("tell worker two to run the tests"), and stop it.

`codex-voice-commands` parses the transcript; this crate executes the result.
It has no app-server or TUI dependency: each machine is an `AgentBackend`, and
an app-server-backed implementation lives in the TUI (see "TUI integration").

```rust
use codex_orchestrator::{Orchestrator, OutcomeStatus};

let mut engine = Orchestrator::new(backends)        // one per machine
    .with_known_machines(tailnet_machine_names)
    .with_known_models(model_refs);
engine.snapshot().await;                            // when voice starts

let outcome = engine.handle_utterance(&transcript, approval_prompt_open).await;
match outcome.status {
    OutcomeStatus::PassThrough => { /* send the transcript to the model */ }
    OutcomeStatus::ForCaller => { /* switch model / attach: outcome.command */ }
    _ => {}
}
speak(&outcome.spoken);   // at most 25 words
show(&outcome.screen);    // one line per agent, recent items, errors
```

## Pieces

| Item | What it is |
|---|---|
| `AgentBackend` | One machine. `machine()`, `list_agents()`, `agent_activity(id)`, `steer(id, message)`, `interrupt(id)`. Methods return `BackendFuture` (a boxed future) so backends can be `Box<dyn AgentBackend>`. |
| `AgentInfo`, `AgentStatus`, `ActivityItem`, `AgentReport` | Engine-owned copies of the protocol data the engine needs. Each field's doc comment names its `codex-app-server-protocol` v2 source (`Thread.id`, `Thread.status`, `Thread.cwd`, `Turn.started_at`, `ThreadItemPayload::CommandExecution`, ...). The protocol crate is not linked. |
| `SteerOutcome`, `InterruptOutcome`, `BackendError` | Results of `turn/steer` / `turn/start` and `turn/interrupt`, and backend failures. |
| `Orchestrator::snapshot()` | Calls `list_agents` on every backend concurrently, each under the timeout (default 3 s, `with_timeout`). A slow or failing machine becomes a `MachineError` entry; the snapshot never fails. |
| `Orchestrator::voice_context()` | The `VoiceContext` for the parser: every agent in the last snapshot as an `AgentRef` (`id` = thread id, `machine` = backend machine), the known machines plus every backend's machine, and the known models. |
| `Orchestrator::parse(transcript, approval_prompt_open)` | `parse_voice_command` plus pronoun resolution (below). |
| `Orchestrator::execute(command)` | Runs one parsed command and returns an `Outcome { status, spoken, screen, command }`. |
| `Orchestrator::handle_utterance(transcript, approval_prompt_open)` | The whole flow: confirm or cancel a pending action, answer a pending "which one?", otherwise parse and execute. |
| `PendingAction`, `confirm()`, `cancel()`, `pending()` | Steer and stop wait for confirmation. |

### Commands

| Command | Engine behaviour |
|---|---|
| `ListAgents` | Fresh snapshot. Speaks a count, then one line per agent (approvals first) while it fits in 25 words, "Plus N more.", and which machines did not answer. The screen lists every agent with its cwd, and every unreachable machine. |
| `AgentStatus` | `agent_activity` on that agent's machine. Speaks its line, plus the first words of its last message when idle. The screen adds the cwd and the newest turn's items (command output: last 5 lines only). |
| `SteerAgent` | Becomes `PendingAction::Steer`; speaks "Tell build on desktop: run the tests. Say confirm." On confirm: `steer`, then "Sent to build on desktop." or, when the backend had to `turn/start`, "Build on desktop was idle, so I started a new turn." |
| `StopAgent` | If the snapshot already shows the agent idle: "Build on desktop is already idle. Nothing to stop.", with nothing pending. Otherwise `PendingAction::Stop`; on confirm `interrupt`, then "Stopped build on desktop." or the same "already idle" reply if the turn finished meanwhile. |
| `Ambiguous` | Speaks "Which agent: build on desktop or build on laptop?" and keeps the question; the next utterance goes to `resolve_choice`. |
| `NotFound` | Speaks "I don't see an agent called deploy." (`Failed`). |
| `ListMachines` | Answered from the context. |
| `SwitchModel`, `AttachMachine` | `ForCaller`: the engine speaks the acknowledgement; the TUI performs it (voice-commands README, "Mapping commands"). |
| `None` | `PassThrough`. |

Failures are per command and per machine: a timeout speaks "Laptop didn't
answer.", a vanished thread "I can't find build on laptop anymore.", and a
machine without a backend "I'm not connected to server."

### Confirmation

Consistent with `is_voice_confirmation` and Lane A's approvals: while an
action is pending, only a confirmation ("confirm", "yes confirm", "I
confirm"; never with "don't", "wait", "maybe", ...) runs it. "Cancel", "no",
"never mind", "stop", ... cancel it and are consumed. Any other utterance
also cancels it ("Cancelled.") and is then handled as a new utterance, so a
prompt still reaches the model. `execute` of a new command replaces whatever
was pending.

### "It" and "that agent"

The engine remembers the last agent the user asked about or acted on (status,
steer, stop, or a resolved choice). When the plain parse finds no command (or
`NotFound`, as for "that agent"), the first of "it", "that agent", "this
agent", "that one", "the same agent" is replaced by that agent's name and the
transcript is parsed again against a context holding only that agent. Only a
status, steer, or stop result counts, so "tell it to run the tests", "stop
it", and "what's it doing" work, while "tell me how it works" stays a prompt.
A named agent always wins over the pronoun, and the steer message keeps its
own "it" ("tell it to retry it").

**Precedence with approvals.** Lane A's approval grammar owns a bare "stop"
and "stop it/this/that" while an approval prompt is open.

1. Approval prompt open: approvals win. The TUI runs its approval handler
   first; `parse` / `handle_utterance` with `approval_prompt_open = true` also
   never turn a pronoun stop into `StopAgent`, and never take "confirm" for a
   pending agent action (it stays pending).
2. No approval prompt: "stop it" (and "stop that agent") means the last
   agent, and like every stop it needs "confirm".
3. A bare "stop" is never an agent command.

### Speech

Every `spoken` string is at most `MAX_SPOKEN_WORDS` (25) words, using
`codex-voice-commands`' `limit_words`, `spoken_response`, `completed_speech`,
and `AgentRef::spoken_label`. Agent lines read like a status board:

- "build on desktop: running cargo test, 2 minutes"
- "tests on laptop: waiting for your approval to run cargo test"
- "docs on laptop: idle"

Commands are read as their first three non-flag words without the shell
wrapper (`/bin/zsh -lc 'cargo test -p codex-core'` → "cargo test
codex-core"). Command output, diffs, reasoning, and code blocks are never
spoken; file changes are read as a file name or "two files". Messages are
read as the first 10 words of their first line, and not at all if that line
starts a code block or diff.

## TUI integration

Not wired yet (needs a `codex-tui` build, which is Mac work). Add
`codex-orchestrator = { workspace = true }` to `tui/Cargo.toml`.

### An app-server-backed `AgentBackend`

`AppServerAgentBackend { machine: String, handle: AppServerRequestHandle }`
in `tui/src/app/voice_orchestrator.rs`. The local machine uses the TUI's own
app-server client (`AppServerSession::request_handle()`); each remote machine
uses the handle of a session connected through Lane C's `ssh://` endpoint:
`codex_app_server_client::resolve_remote_addr("ssh://<host>")` gives
`RemoteAppServerEndpoint::Ssh`, `RemoteAppServerClient::connect(..)` (as
`app_server_connection::connect` does for `AppServerTarget::Remote`) connects
it, and `BackendRegistry::request_handle(owner)` returns its handle once
registered. Name each backend after its host (`codex_tailnet::Machine::name`),
or the local hostname.

| Method | RPCs (`ClientRequest` variant → method) |
|---|---|
| `list_agents` | `ThreadLoadedList` → `thread/loaded/list` (`ThreadLoadedListParams { cursor, limit }`, follow `next_cursor`), then `ThreadRead` → `thread/read` (`include_turns: false`) per id for `Thread`. Add recent unloaded threads from `ThreadList` → `thread/list` (`limit: 20`) as `NotLoaded`. For each active thread, `ThreadTurnsList` → `thread/turns/list` (`limit: 1`, `sort_direction: Desc`, `items_view: Some(TurnItemsView::Full)`) gives `turn_started_at` and `latest` (last item). Map `Thread` → `AgentInfo` per the field docs in `src/agent.rs`. |
| `agent_activity` | `thread/read` (`include_turns: false`) for `agent`, then `thread/turns/list` (`limit: 1`, `sort_direction: Desc`, `items_view: Some(TurnItemsView::Full)`) for `recent`. Older servers: `thread/read` with `include_turns: true` and take the last turn. Map each `ThreadItemPayload` to an `ActivityItem` (`CommandExecution` keeps `command`, `status`, `exit_code`, `aggregated_output`; `FileChange` keeps only the paths). |
| `steer` | Find the in-progress turn (as `App::active_turn_id_for_thread` does, else the `TurnStatus::InProgress` turn from `thread/turns/list`). If there is one: `TurnSteer` → `turn/steer` (`TurnSteerParams { thread_id, expected_turn_id: turn_id, input: vec![UserInput::Text { text: message, .. }], client_user_message_id: Some(uuid), .. }`) → `Steered { turn_id }`. If not: `TurnStart` → `turn/start` with the same input, the path a typed message to that thread uses → `Started { turn_id: response.turn.id }`. |
| `interrupt` | Same turn lookup; none → `AlreadyIdle`. Otherwise `TurnInterrupt` → `turn/interrupt` (`TurnInterruptParams { thread_id, turn_id }`), as `App::stop_agents_overview_thread` does → `Interrupted { turn_id }`. |

Map app-server errors: transport/connection failures → `Unreachable`, an
unknown thread id → `AgentNotFound`, anything else → `Request`.

### Where `execute` is called in the voice flow

Follow the voice-commands README ("TUI integration", step 1), with the
engine in place of hand-written command handling:

1. `ChatWidget::on_realtime_transcript_done` → `on_realtime_user_utterance`
   (`tui/src/chatwidget/realtime/voice_status.rs`). Approvals run first
   (`handle_realtime_voice_approval`); if they decline the utterance, send
   `AppEvent::VoiceCommand { transcript, approval_prompt_open }` and hide the
   transcript until the answer comes back. `ChatWidget` cannot make
   app-server requests; `App` can.
2. In `tui/src/app/event_dispatch.rs`, `App` owns the `Orchestrator` (built
   when voice starts, after `codex_tailnet::discover` and one
   `snapshot()`), calls `handle_utterance(&transcript, approval_prompt_open)`
   and:
   - speaks `outcome.spoken` through `ChatWidget::speak_realtime_status`
     (via a small `speak_voice_command_result(String)`), and shows
     `outcome.screen` as an info cell;
   - `PassThrough`: submit the transcript to the model as before (also speak
     `spoken` when it is "Cancelled.");
   - `ForCaller`: run the `SwitchModel` / `AttachMachine` mapping from the
     voice-commands README.
3. Delegated `<realtime_delegation>` copies are never executed: the
   delegation check in `consume_realtime_voice_control_handoff` swallows any
   transcript for which `engine.parse(text, approval_open).is_command()`,
   and only the user's own transcript reaches `handle_utterance`.
4. Refresh the snapshot on thread start/finish notifications, or before
   `ListAgents` (which `execute` already does).

## Gaps

- Never run against a real app-server; only the in-memory `FakeBackend`.
- Agents are matched by thread id; two machines with the same thread id
  (forks of one rollout) would confuse `resolve_choice`, which looks agents
  up by id alone.
- No live updates: the snapshot is refreshed on `ListAgents` or by the
  caller.
- Bazel: `BUILD.bazel` is added, but `MODULE.bazel.lock` was not
  regenerated.
