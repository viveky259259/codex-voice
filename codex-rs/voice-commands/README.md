# codex-voice-commands

A pure parser that turns one final voice transcript into an orchestration,
model, or machine command for voice-first Codex. No I/O, no async, no TUI
types: the TUI builds a `VoiceContext`, calls `parse_voice_command`, runs the
result, and speaks the strings this crate produces.

```rust
use codex_voice_commands::{parse_voice_command, spoken_response, VoiceCommand, VoiceContext};

let command = parse_voice_command("tell the desktop agent to run the tests", &context);
if command.is_command() {
    if let Some(speech) = spoken_response(&command, &context) {
        // speak it: "Tell build on desktop: run the tests. Say confirm."
    }
}
```

## Grammar

Matching is conservative. An utterance becomes a command only when it has one
of the shapes below **and** its target resolves against the context. Anything
else is `VoiceCommand::None` and goes to the model as a normal prompt.

Before matching, the transcript is lowercased and split into words.
Punctuation, hyphens, and underscores separate words, and "what's"/"how's"
become "what is"/"how is". Leading fillers ("ok", "okay", "um", "hey",
"codex", "please", "can you", "could you", ...) and trailing fillers
("please", "thanks", "thank you", "now", "right now", "for me") are dropped.
Utterances other than steering are capped at 12 words.

| Command | Shapes | Notes |
|---|---|---|
| `ListAgents` | "what's running", "list (my/the/all) agents", "show (me) my agents", "what agents are running", "agent status" | Exact phrases only: "what's running on port 3000" is a prompt. |
| `AgentStatus { agent }` | "what's X doing", "what is X working on", "what's X up to", "how's X doing", "(what's the) status of X", "check on X" | |
| `SteerAgent { agent, message, needs_confirmation: true }` | "tell X to M", "ask X to M" | `M` keeps the original casing and punctuation ("fix parser.rs"). Every "to" is tried, and the longest target that resolves wins. |
| `StopAgent { agent, needs_confirmation: true }` | "stop X", "interrupt X" | A bare "stop", and "stop it/this/that", stay with the approval grammar (Lane A). |
| `SwitchModel { slug }` | "switch to M", "use M", "use model M", "swap to M", "switch/change/set (the) model to M" | Resolved through `ModelRef::spoken_aliases` and the slug. |
| `ListMachines` | "list my machines", "show my machines", "what machines do I have", "which machines are online" | |
| `AttachMachine { machine }` | "connect (me) to H", "attach (me) to H", "ssh into H", "ssh to H" | Machines are `known_machines` plus every `AgentRef::machine`. |
| `Ambiguous { action, heard, candidates }` | any of the above | The target matched more than one thing. Never guessed. |
| `NotFound { action, heard }` | any of the above | Only when the utterance names the kind explicitly ("the deploy agent", "switch the model to llama", "my gaming machine"). Otherwise it is `None`. |

### Agent references (X)

- By name: "build", "the build agent", "my build", "agent build".
- By machine: "desktop", "the desktop agent", "the agent on my laptop",
  "the agent running on the laptop".
- Name and machine: "build on laptop", "the build agent on the desktop".
- "the agent" alone means every agent. With one agent it resolves; with
  several it is `Ambiguous`.

### Name matching

1. **Exact key match.** Lowercase, separators removed, number words read as
   digits, and "point"/"dot" between digits dropped. So "mac studio",
   "MacStudio", "mac-studio", and "mac_studio" are equal, "worker two" equals
   "worker-2", and "gpt five point five" equals "gpt-5.5". Leading
   "my"/"the"/"our" and a trailing kind noun ("agent", "model", "machine",
   "computer", "box") are ignored.
2. **Partial match (agents and models only).** If nothing matches exactly,
   every spoken word must appear in the name: "frontend" matches
   "build-frontend" and "opus" matches "claude-opus-5-5". A phrase made only of
   generic words ("the server", "it", "this") or only numbers never
   partially matches.
3. Machines for `AttachMachine` need an exact key match, so "connect to the
   database" never picks a machine called `database-host`.
4. More than one match gives `Ambiguous`. `resolve_choice(action, candidates,
   answer, context)` reads the follow-up answer: an ordinal ("the second
   one", "number two") or enough of a label ("the one on laptop").

### Confirmation

`SteerAgent` and `StopAgent` carry `needs_confirmation: true`
(`VoiceCommand::needs_confirmation()`), matching Lane A's approval flow:
speak the `spoken_response` question ("Stop tests on laptop? Say confirm.").
Then only an utterance for which `is_voice_confirmation` is true runs the
command, and any other utterance cancels it. `is_voice_confirmation` uses the
same phrases and negation rules as `voice_approval.rs`: "confirm", "yes
confirm", "I confirm", and never anything with "don't", "wait", "maybe", ...

### Speech

Every string is at most `MAX_SPOKEN_WORDS` (25) words. Names are read with
separators and camelCase humps as spaces ("Mac Studio", "gpt 5.4 mini").

- `spoken_response(command, context)`: what to say right after parsing.
  Lists are answered from the context ("Two agents running: build on desktop,
  tests on laptop."; more than three become "..., and nine more"). Destructive
  commands ask for confirmation. Other commands get an acknowledgement
  ("Checking build on desktop.", "Switching to opus.", "Connecting to Mac
  Studio."), a question ("Which agent: build on desktop or build on laptop?"),
  or "I don't see an agent called deploy."
- `completed_speech(command, context)`: after the command ran. For example,
  "Sent to build on desktop.", "Stopped tests on laptop.", "Now using
  sonnet.", or "Connected to Mac Studio."
- `agent_status_speech(agent, activity, latest)`: the answer to
  `AgentStatus`, for example "Build on desktop is working. Latest: Running
  cargo test -p codex-core."

## TUI integration

This was written, not compiled, in the cloud container, which cannot build
`codex-tui`. Wire it up on the Mac, add `codex-voice-commands = { workspace =
true }` to `tui/Cargo.toml`, and run `cargo test -p codex-tui --lib -- realtime
voice` with `RUST_MIN_STACK=8388608`.

### 1. Where final transcripts arrive

- `ChatWidget::on_realtime_transcript_delta` (`tui/src/chatwidget/realtime.rs`)
  only accumulates partial captions. Do not parse here.
- `ChatWidget::on_realtime_transcript_done` ends with
  `self.on_realtime_user_utterance(&text, pre_utterance_generation)` for
  final `user` transcripts. That function is in
  `tui/src/chatwidget/realtime/voice_status.rs`. Add a step after
  `handle_realtime_voice_approval` and before the `else` branch. Approvals
  stay first, so a bare "stop" is still Lane A's abort.

  ```rust
  } else if self.handle_realtime_voice_command(text) {
      self.keep_in_flight_voice_turns(pre_utterance_generation);
  } else { ... }
  ```

  `handle_realtime_voice_command` should do the following:
  - If a pending voice command exists, check `is_voice_confirmation(text)`.
    When it is true, run the command; otherwise clear the pending command,
    speak "Cancelled.", and return false.
  - If a pending `Ambiguous` exists, call `resolve_choice`.
  - Otherwise call `parse_voice_command(text, &self.voice_context())`. If it
    returns `None`, return false. If it returns a command, speak
    `spoken_response(..)` with `speak_realtime_status`, store the command
    when `needs_confirmation()` is true, otherwise send it, and return true.

  Store the pending command beside `voice_approval` in `VoiceControlState`
  and reset it with the call.
- **The delegation path.** The realtime model may also hand the same
  utterance off as a `<realtime_delegation>` turn.
  `note_realtime_user_item_started` (realtime.rs, around line 712) calls
  `consume_realtime_voice_control_handoff(turn_id, input)`, which matches
  only `parse_voice_approval_intent(&display)`. Add a second check:
  `parse_voice_command(&display, &ctx).is_command()`. A handed-off command
  is **never executed**, in line with Lane A's rule that only the user's own
  transcript may arm or confirm an approval. Always swallow it (return true,
  so it is hidden and does not become a second prompt), in either arrival
  order. Only the user's transcript runs the command. While a voice command
  is pending, also swallow a handed-off "confirm", "yes", or "no" without
  acting on it; Lane A's handoff code holds a "confirm" back only while an
  approval prompt is open. The known gap from Lane A applies here too: core
  has already appended the handed-off text to the turn.
- `ChatWidget` cannot make app-server requests. Add an
  `AppEvent::VoiceCommand { thread_id, command: codex_voice_commands::VoiceCommand }`
  in `tui/src/app_event.rs`, send it with `self.app_event_tx.send(..)`, and
  handle it in `tui/src/app/event_dispatch.rs`, where `app_server:
  &mut AppServerSession` is available. Return the speech to the widget with
  the existing `speak_realtime_status` (for example through a small
  `ChatWidget::speak_voice_command_result(String)`).

### 2. Building the `VoiceContext`

- `known_agents`: the threads the agents overview already tracks
  (`tui/src/app/agents_overview*.rs`, `loaded_threads.rs`, and
  `backend_registry`). Use `id = thread.id`, `name = thread.name` (else
  `agent_nickname`, else a short `preview`), and `machine` = the backend's
  host label for remote backends (the `ssh://` host from Lane C), or `None`
  for the local backend. App-server source: `thread/loaded/list`
  (`AppServerSession::thread_loaded_list`, returning ids), then `thread/read`
  (`AppServerSession::thread_read(id, false)`) for name, status, and preview.
  For remote backends, use `BackendRegistry::request_handle(owner)`.
- `known_models`: `self.model_catalog.models` (`ModelPreset`). Use `slug =
  preset.model` and `spoken_aliases = [preset.display_name]`, plus a small
  alias table (for example "opus", "claude", "sonnet", "gpt five"). Lane B's
  `anthropic-proxy/model_catalog.example.json` supplies the Claude entries.
- `known_machines`: `codex_tailnet::discover(&TailscaleCli::from_env(),
  SshConfigSource::from_home().as_ref())` (Lane C, async). Cache `Machine::name`
  and refresh it when voice starts, never per utterance.

### 3. Mapping commands to app-server calls

| Command | Call |
|---|---|
| `ListAgents` | Speak `spoken_response`, which uses the context. To refresh first, call `thread/loaded/list` (`thread_loaded_list(ThreadLoadedListParams::default())`), then `thread/read` for each id. |
| `AgentStatus { agent }` | `thread/read` (`app_server.thread_read(thread_id, /*include_turns*/ false)`). Map `Thread.status` to `AgentActivity`: `NotLoaded`→`NotLoaded`, `Idle`→`Idle`, `SystemError`→`Error`, `Active { active_flags }`→`WaitingOnApproval` / `WaitingOnUserInput` if that flag is set, else `Working`. For `latest`, use the last agent message: for paginated history, `thread/turns/list` (`app_server.thread_turns_page(thread_id, None, 1)`); otherwise `thread_read(id, true)`. Then speak `agent_status_speech(..)`. |
| `SteerAgent { agent, message }` | After confirmation, look up the active turn the way `stop_agents_overview_thread` (`agents_overview.rs`) does: `active_turn_id_for_thread`, or else `thread_read` / `thread_turns_page` and pick the `TurnStatus::InProgress` turn. Then call `turn/steer`: `app_server.turn_steer(thread_id, turn_id, client_user_message_id, vec![UserInput::Text { text: message, .. }])`. If no turn is running, start one with `turn/start` (the same path a typed message to that thread uses) instead of failing. |
| `StopAgent { agent }` | After confirmation, call `App::stop_agents_overview_thread(app_server, thread_id)`. It already resolves the active turn and calls `turn/interrupt` (`app_server.turn_interrupt(thread_id, turn_id)`). Then speak `completed_speech`. |
| `SwitchModel { slug }` | The model-picker path: send `AppEvent::SelectSessionModel { model: slug, effort: None, service_tier: None }` for this session only (handled by `App::select_session_model` in `app/model_defaults.rs`), or `AppEvent::ApplyModelSelection { model, effort: None }` to also save the default. Either way the existing Astra/Daybreak checks run. |
| `ListMachines` | Speak `spoken_response` from the cached `codex_tailnet::Discovery`. |
| `AttachMachine { machine }` | Lane C's endpoint: find the `codex_tailnet::Machine` whose `name` matches and build `ssh://{machine.ssh_destination}`. `codex_app_server_client::resolve_remote_addr(&addr)` gives a `RemoteAppServerEndpoint`; `connect_remote_app_server(endpoint)` (as `app_server_connection::connect` does for `AppServerTarget::Remote`) connects it. Register the new session with `backend_registry.connect(&session)` and open it with the existing other-backend path (`App::start_new_session_on_other_backend` / `switch_backend_context` in `app/backend_selection.rs`). Until that runtime attach exists, speak "Run codex remote attach <machine>." |
| `Ambiguous` | Speak the question, store `(action, candidates)`, and pass the next utterance to `resolve_choice`. Drop the command if that returns `None`. |
| `NotFound` | Speak `spoken_response`. Nothing is sent to the model. |

### Tests to add in `codex-tui`

- `on_realtime_user_utterance("stop build")` speaks the confirmation and sends
  nothing. A following "confirm" emits `AppEvent::VoiceCommand(StopAgent)`;
  any other utterance cancels the command.
- A bare "stop" while an approval prompt is open still aborts the approval
  (Lane A precedence).
- "tell me how this function works" goes to the model unchanged.
- A delegated `<realtime_delegation>` copy of "what's build doing" is
  consumed, not started as a turn.
