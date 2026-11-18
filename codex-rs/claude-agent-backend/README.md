# codex-claude-agent-backend

Claude Code agent sessions in the Codex TUI, the way T3 Code runs them: the
`claude` CLI is driven over its streaming JSON input/output and the Agent SDK
control protocol, and authenticated by the CLI's own login (no API key).

`start(ClaudeCodeOptions)` returns an `AppServerClient::AgentBackend`. It answers
the app-server requests the TUI sends and emits the notifications and server
requests a Codex app server would, so the TUI, its approval overlay and the
voice layer work unchanged. Nothing in core or app-server changes.

Start it with `codex --agent claude-code [--claude-bin PATH] [-m MODEL]`.

## Wire format

Taken from the published SDK, `@anthropic-ai/claude-agent-sdk` **0.3.327**
(`npm pack`; read from `sdk.d.ts` and the argv/control code in `core.mjs`, not
run). One JSON object per line on stdin and stdout.

### argv

The SDK builds, for a streaming `query()` with `canUseTool`,
`includePartialMessages`, adaptive summarized thinking and permission mode
`default` (the options T3 Code passes), in this order:

```
claude --output-format stream-json --verbose --input-format stream-json
       --thinking=adaptive --thinking-display=summarized
       [--model=<id>]
       --permission-prompt-tool=stdio
       [--resume=<session id>]
       --permission-mode=default
       --include-partial-messages
       [--session-id=<uuid>]              # new sessions: we pick the id
       --settings {"showThinkingSummaries":true}
```

The SDK writes valued flags as `--flag=value`, and extra args as two words
unless the value starts with `-`. There is no `--print`. A new thread's id is
the session id we pass with `--session-id`; resuming passes it to `--resume`.

### Client → CLI (stdin)

| Message | Shape |
|---|---|
| initialize (first line) | `{"type":"control_request","request_id":"init-1","request":{"subtype":"initialize"}}` (we register no hooks, SDK MCP servers or agents) |
| user turn | `{"type":"user","session_id":"","message":{"role":"user","content":[{"type":"text","text":"..."}]},"parent_tool_use_id":null}` |
| interrupt | `{"type":"control_request","request_id":ID,"request":{"subtype":"interrupt"}}` |
| switch model | `{"type":"control_request","request_id":ID,"request":{"subtype":"set_model","model":"sonnet"}}` (`null` = Claude Code's default) |
| permission answer | `{"type":"control_response","response":{"subtype":"success","request_id":<can_use_tool id>,"response":PermissionResult}}` |
| refuse a control request | `{"type":"control_response","response":{"subtype":"error","request_id":ID,"error":"..."}}` |

`PermissionResult` (camelCase on the wire; the SDK spreads it and adds `toolUseID`):

```json
{"behavior":"allow","updatedInput":{...tool input...},"updatedPermissions":[PermissionUpdate],"toolUseID":"toolu_..."}
{"behavior":"deny","message":"User declined tool execution.","interrupt":true,"toolUseID":"toolu_..."}
```

`PermissionUpdate` for "allow for this session":
`{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"..."}],"behavior":"allow","destination":"session"}`.

### CLI → client (stdout)

| `type` | Used for |
|---|---|
| `control_response` `{response:{subtype:"success"\|"error",request_id,response?,error?}}` | the initialize reply: `models: ModelInfo[]` (`value`, `resolvedModel?`, `displayName`, `description`, `supportsEffort?`, `supportedEffortLevels?`), `account` (`email?`, `organization?`, `subscriptionType?`, `tokenSource?`, `apiKeySource?`, `apiProvider?`), `claude_code_version?` |
| `control_request` `{request_id, request:{subtype:"can_use_tool", tool_name, input, tool_use_id, permission_suggestions?, blocked_path?, decision_reason?, title?, display_name?, description?}}` | approvals |
| `control_cancel_request` `{request_id}` | the CLI withdrew a permission request |
| `system` `subtype:"init"` (`session_id`, `model`, `tools`, `claude_code_version`, ...) | version |
| `stream_event` `{event: BetaRawMessageStreamEvent, parent_tool_use_id}` | `message_start`, `content_block_start/delta/stop` with `text_delta` / `thinking_delta` |
| `assistant` `{message: BetaMessage, parent_tool_use_id}` | per-block snapshots; `tool_use` blocks |
| `user` `{message:{content:[{type:"tool_result",tool_use_id,content,is_error}]}}` | tool results |
| `result` `{subtype:"success"\|"error_during_execution"\|"error_max_turns"\|..., is_error, result?, errors?, usage}` | end of turn |

## Mapping

| Claude Code | App-server |
|---|---|
| `text_delta` blocks | `item/started` + `item/agentMessage/delta` + `item/completed` (`agentMessage`); snapshots that did not stream are shown whole |
| `thinking_delta` blocks | `reasoning` item with `item/reasoning/summaryTextDelta` |
| `Bash` tool_use / tool_result | `commandExecution` started / completed with the output (exit code 0, or 1 when `is_error`) |
| `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | `fileChange` with paths and a unified diff computed from the file and the old/new strings before the tool runs; just the path when the old text is not found |
| `mcp__<server>__<tool>` | `mcpToolCall` |
| any other tool | `dynamicToolCall` |
| `result` | `thread/tokenUsage/updated` + `turn/completed` (`failed` with the CLI's errors, `interrupted` after `turn/interrupt`) |
| `can_use_tool` for `Bash` | `item/commandExecution/requestApproval` |
| `can_use_tool` for edit tools | `item/fileChange/requestApproval` |
| `can_use_tool` for `AskUserQuestion` | `item/tool/requestUserInput`; answers return as `updatedInput.answers` keyed by question text |
| other `can_use_tool` | `item/permissions/requestApproval` asking for the network (web tools) or the tool's path, so the overlay's grant/deny is meaningful |
| Accept / AcceptForSession / Decline / Cancel | allow / allow + session rules (the CLI's suggestions moved to `session`, else an allow rule for the tool) / deny / deny with `interrupt: true` |
| `initialize` `models` | `model/list`: the CLI's models first, then the built-in fallback (`claude-fable-5-2`, `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`) for ids it did not list. Each family name (opus, sonnet, haiku, fable) belongs to its first listed model. |
| `/model`, "switch to sonnet" (`thread/settings/update` `model`) | `set_model` on the running CLI (no restart) |
| `initialize` `account` without a login | a warning to run `claude` and `/login` |

`account/read` reports no OpenAI account and `requiresOpenaiAuth: false`, so
the TUI skips its login screen. Requests not listed here are refused with
`-32601` and a message naming Claude Code.

Not mapped: images and other non-text input, steering a running turn,
history replay on resume (the CLI keeps it; the TUI shows only new turns),
fork/rollback/compaction, effort changes mid-session, subagent transcripts,
Codex sandbox policies and execpolicy amendments (Claude Code enforces its own
permissions; `--permission-mode=default` makes it ask).

## Tests

`cargo test -p codex-claude-agent-backend`. The integration tests run
`tests/fake_claude.py` with `python3 -I` as `claude`; the real CLI is never run.

## Credits

The adapter design follows T3 Code (pingdotgg/t3code, MIT),
`apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts` and
`apps/server/src/provider/ClaudeModelCatalog.ts`.
