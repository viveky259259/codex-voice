# A stand-in for the `claude` CLI that speaks the stream-json and SDK control
# protocol documented in README.md. Tests run it with `python3 -I`; it never
# touches the network or any Claude credentials.
#
# FAKE_CLAUDE_LOG       file that receives {"argv": [...]} and {"stdin": {...}} lines
# FAKE_CLAUDE_SCENARIO  text | bash | edit | interrupt
import json
import os
import sys

LOG = os.environ["FAKE_CLAUDE_LOG"]
SCENARIO = os.environ.get("FAKE_CLAUDE_SCENARIO", "text")
SESSION = next(
    (arg.split("=", 1)[1] for arg in sys.argv if arg.startswith(("--session-id=", "--resume="))),
    "",
)


def log(entry):
    with open(LOG, "a") as handle:
        handle.write(json.dumps(entry) + "\n")


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    message = json.loads(line)
    log({"stdin": message})
    return message


def answer(request_id, response):
    send({"type": "control_response",
          "response": {"subtype": "success", "request_id": request_id, "response": response}})


def handle_control(message):
    """Answers our control requests; returns the request's subtype."""
    subtype = message["request"]["subtype"]
    if subtype == "initialize":
        answer(message["request_id"], {
            "commands": [], "agents": [], "output_style": "default", "available_output_styles": [],
            "models": [
                {"value": "default", "displayName": "Default (recommended)", "description": "Claude Code's default"},
                {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus",
                 "description": "Opus 5.5", "supportsEffort": True,
                 "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
                {"value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet",
                 "description": "Sonnet 5.5"},
            ],
            "account": {"email": "dev@example.com", "subscriptionType": "max", "apiProvider": "firstParty"},
            "claude_code_version": "9.9.9-fake",
        })
    else:
        answer(message["request_id"], {})
    return subtype


def stream(event):
    send({"type": "stream_event", "event": event, "parent_tool_use_id": None,
          "uuid": "u", "session_id": SESSION})


def assistant(message_id, block):
    send({"type": "assistant", "message": {"id": message_id, "role": "assistant", "content": [block]},
          "parent_tool_use_id": None, "uuid": "u", "session_id": SESSION})


def result(subtype="success", is_error=False, errors=None):
    message = {"type": "result", "subtype": subtype, "is_error": is_error, "num_turns": 1,
               "result": "", "duration_ms": 1, "duration_api_ms": 1, "total_cost_usd": 0,
               "stop_reason": "end_turn", "session_id": SESSION, "uuid": "u",
               "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100,
                         "cache_creation_input_tokens": 0}}
    if errors:
        message["errors"] = errors
    send(message)


def text_reply(message_id, text):
    stream({"type": "message_start", "message": {"id": message_id}})
    stream({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
    stream({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})
    stream({"type": "content_block_stop", "index": 0})
    assistant(message_id, {"type": "text", "text": text})


def wait_for_permission(request_id):
    while True:
        message = read()
        if message.get("type") == "control_response" and message["response"]["request_id"] == request_id:
            return message["response"]["response"]
        if message.get("type") == "control_request":
            handle_control(message)


def tool_turn(name, tool_input, output):
    tool_use_id = "toolu_" + name.lower()
    assistant("msg_tool", {"type": "tool_use", "id": tool_use_id, "name": name, "input": tool_input})
    send({"type": "control_request", "request_id": "perm-1",
          "request": {"subtype": "can_use_tool", "tool_name": name, "input": tool_input,
                      "tool_use_id": tool_use_id, "permission_suggestions": []}})
    decision = wait_for_permission("perm-1")
    if decision["behavior"] == "allow":
        content, is_error = output, False
    else:
        content, is_error = decision["message"], True
    send({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}]},
        "parent_tool_use_id": None, "uuid": "u", "session_id": SESSION})
    if decision.get("interrupt"):
        result("error_during_execution", True)
        return
    text_reply("msg_after", "Done." if not is_error else "Skipped.")
    result()


def run_turn():
    send({"type": "system", "subtype": "init", "session_id": SESSION, "model": "claude-opus-5-5",
          "claude_code_version": "9.9.9-fake", "tools": ["Bash", "Edit", "Write"], "cwd": os.getcwd(),
          "permissionMode": "default", "apiKeySource": "none", "mcp_servers": [],
          "slash_commands": [], "output_style": "default", "skills": [], "plugins": []})
    if SCENARIO == "text":
        stream({"type": "message_start", "message": {"id": "msg_1"}})
        stream({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})
        stream({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Thinking it over."}})
        stream({"type": "content_block_stop", "index": 0})
        assistant("msg_1", {"type": "thinking", "thinking": "Thinking it over."})
        stream({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})
        stream({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hello"}})
        stream({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": " from Claude."}})
        stream({"type": "content_block_stop", "index": 1})
        assistant("msg_1", {"type": "text", "text": "Hello from Claude."})
        result()
    elif SCENARIO == "bash":
        tool_turn("Bash", {"command": "echo hi", "description": "Print hi"}, "hi")
    elif SCENARIO == "edit":
        tool_turn("Write", {"file_path": os.path.join(os.getcwd(), "notes.md"), "content": "hello\n"}, "written")
    elif SCENARIO == "interrupt":
        stream({"type": "message_start", "message": {"id": "msg_long"}})
        stream({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        stream({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Working"}})
        while True:
            message = read()
            if message.get("type") == "control_request" and handle_control(message) == "interrupt":
                result("error_during_execution", True)
                return


log({"argv": sys.argv[1:]})
while True:
    message = read()
    if message.get("type") == "control_request":
        handle_control(message)
    elif message.get("type") == "user":
        run_turn()
