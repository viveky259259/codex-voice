use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn no_files(_: &Path) -> Option<String> {
    None
}

fn mapper() -> TurnMapper {
    TurnMapper::new("thread-1", "turn-1", Path::new("/repo"))
}

fn feed(mapper: &mut TurnMapper, messages: &[Value]) -> Vec<ServerNotification> {
    messages
        .iter()
        .flat_map(|message| mapper.on_message(message, 5, &no_files))
        .collect()
}

fn stream(event: Value) -> Value {
    json!({"type": "stream_event", "event": event, "parent_tool_use_id": null, "session_id": "s"})
}

/// Method plus the item id or delta, for compact assertions.
fn summary(notifications: &[ServerNotification]) -> Vec<String> {
    notifications
        .iter()
        .map(|notification| match notification {
            ServerNotification::ItemStarted(n) => {
                format!("started {}", item_label(&n.item.payload))
            }
            ServerNotification::ItemCompleted(n) => {
                format!("completed {}", item_label(&n.item.payload))
            }
            ServerNotification::AgentMessageDelta(n) => format!("text {} {:?}", n.item_id, n.delta),
            ServerNotification::ReasoningSummaryTextDelta(n) => {
                format!("thinking {} {:?}", n.item_id, n.delta)
            }
            other => format!("{other:?}"),
        })
        .collect()
}

fn item_label(payload: &ThreadItemPayload) -> String {
    match payload {
        ThreadItemPayload::AgentMessage { id, text, .. } => format!("agentMessage {id} {text:?}"),
        ThreadItemPayload::Reasoning { id, summary, .. } => format!("reasoning {id} {summary:?}"),
        ThreadItemPayload::CommandExecution {
            id,
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } => format!("command {id} {command:?} {status:?} {aggregated_output:?} {exit_code:?}"),
        ThreadItemPayload::FileChange {
            id,
            changes,
            status,
        } => format!(
            "fileChange {id} {:?} {status:?}",
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>()
        ),
        ThreadItemPayload::McpToolCall {
            id,
            server,
            tool,
            status,
            ..
        } => format!("mcp {id} {server}/{tool} {status:?}"),
        ThreadItemPayload::DynamicToolCall {
            id,
            tool,
            status,
            success,
            ..
        } => format!("dynamic {id} {tool} {status:?} {success:?}"),
        other => format!("{other:?}"),
    }
}

#[test]
fn streamed_thinking_and_text_become_reasoning_and_agent_message_items() {
    let mut mapper = mapper();
    let out = feed(
        &mut mapper,
        &[
            stream(json!({"type": "message_start", "message": {"id": "msg_1"}})),
            stream(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            ),
            stream(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Plan."}}),
            ),
            stream(json!({"type": "content_block_stop", "index": 0})),
            stream(
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            ),
            stream(
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hello"}}),
            ),
            stream(
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": " there"}}),
            ),
            stream(json!({"type": "content_block_stop", "index": 1})),
            // The snapshots of blocks that already streamed add nothing.
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_1", "content": [{"type": "thinking", "thinking": "Plan."}]}}),
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_1", "content": [{"type": "text", "text": "Hello there"}]}}),
        ],
    );
    assert_eq!(
        summary(&out),
        vec![
            "started reasoning msg_1:thinking:0 []",
            "thinking msg_1:thinking:0 \"Plan.\"",
            "completed reasoning msg_1:thinking:0 [\"Plan.\"]",
            "started agentMessage msg_1:text:0 \"\"",
            "text msg_1:text:0 \"Hello\"",
            "text msg_1:text:0 \" there\"",
            "completed agentMessage msg_1:text:0 \"Hello there\"",
        ]
    );
}

#[test]
fn an_unstreamed_snapshot_is_shown_whole() {
    let mut mapper = mapper();
    let out = feed(
        &mut mapper,
        &[
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_2", "content": [{"type": "text", "text": "Done."}]}}),
        ],
    );
    assert_eq!(
        summary(&out),
        vec![
            "started agentMessage msg_2:text:0 \"\"",
            "completed agentMessage msg_2:text:0 \"Done.\"",
        ]
    );
}

#[test]
fn bash_tool_use_and_result_become_a_command_execution() {
    let mut mapper = mapper();
    let out = feed(
        &mut mapper,
        &[
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_3", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}]}}),
            json!({"type": "user", "parent_tool_use_id": null, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt", "is_error": false}]}}),
        ],
    );
    assert_eq!(
        summary(&out),
        vec![
            "started command toolu_1 \"ls\" InProgress None None",
            "completed command toolu_1 \"ls\" Completed Some(\"a.txt\\nb.txt\") Some(0)",
        ]
    );
}

#[test]
fn tools_map_by_name_and_a_declined_edit_is_declined() {
    let mut mapper = mapper();
    let mut out = feed(
        &mut mapper,
        &[
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_4", "content": [
            {"type": "tool_use", "id": "t_edit", "name": "Edit", "input": {"file_path": "/repo/a.rs", "old_string": "a", "new_string": "b"}},
            {"type": "tool_use", "id": "t_mcp", "name": "mcp__github__get_issue", "input": {"number": 1}},
            {"type": "tool_use", "id": "t_web", "name": "WebFetch", "input": {"url": "https://example.com"}}]}}),
        ],
    );
    mapper.mark_declined("t_edit");
    out.extend(feed(
        &mut mapper,
        &[json!({"type": "user", "parent_tool_use_id": null, "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t_edit", "content": "User declined tool execution.", "is_error": true},
            {"type": "tool_result", "tool_use_id": "t_mcp", "content": [{"type": "text", "text": "issue"}]},
            {"type": "tool_result", "tool_use_id": "t_web", "content": "failed", "is_error": true}]}})],
    ));
    assert_eq!(
        summary(&out),
        vec![
            "started fileChange t_edit [\"/repo/a.rs\"] InProgress",
            "started mcp t_mcp github/get_issue InProgress",
            "started dynamic t_web WebFetch InProgress None",
            "completed fileChange t_edit [\"/repo/a.rs\"] Declined",
            "completed mcp t_mcp github/get_issue Completed",
            "completed dynamic t_web WebFetch Failed Some(false)",
        ]
    );
}

#[test]
fn subagent_output_is_not_shown_and_finish_closes_open_items() {
    let mut mapper = mapper();
    let out = feed(
        &mut mapper,
        &[
            json!({"type": "assistant", "parent_tool_use_id": "toolu_task", "message": {"id": "sub", "content": [{"type": "text", "text": "inner"}]}}),
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "msg_5", "content": [
                {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {"command": "sleep 60"}}]}}),
        ],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        summary(&mapper.finish(9)),
        vec![
            "completed command toolu_2 \"sleep 60\" Failed Some(\"The turn ended before this tool finished.\") Some(1)"
        ]
    );
}

#[test]
fn result_usage_and_errors() {
    let usage = usage_breakdown(
        &json!({"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20}),
    );
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_input_tokens,
            usage.output_tokens,
            usage.total_tokens
        ),
        (130, 100, 20, 5, 135)
    );
    assert_eq!(
        result_error(&json!({"type": "result", "subtype": "success", "is_error": false})),
        None
    );
    assert_eq!(
        result_error(&json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["API Error: 401"]}))
            .map(|error| error.message),
        Some("API Error: 401".to_string())
    );
}

#[test]
fn the_last_text_block_is_the_turns_answer() {
    let mut mapper = mapper();
    assert_eq!(mapper.last_agent_message(), None);
    feed(
        &mut mapper,
        &[
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m1", "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
            ]}}),
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m2", "content": [
                {"type": "thinking", "thinking": "Done looking."},
                {"type": "text", "text": "There are two files."},
            ]}}),
        ],
    );
    let answer = mapper.last_agent_message().expect("an answer");
    assert_eq!(
        item_label(&answer.payload),
        "agentMessage m2:text:0 \"There are two files.\""
    );
    assert_eq!(answer.completed_at_ms, Some(5));
}
