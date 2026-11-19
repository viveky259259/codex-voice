"""List the voice models codex-voice can use: `codex-voice models`.

Speech and transcription models come from the local-models MCP gateway
(local-agents' mcp_server.py), Kokoro voices from the Hugging Face cache, and
macOS voices from `say -v '?'`. Standard library only.
"""

import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ACCEPT = "application/json, text/event-stream"


def post(url, payload, session=None):
    headers = {"content-type": "application/json", "accept": ACCEPT}
    if session:
        headers["mcp-session-id"] = session
    request = urllib.request.Request(
        url, data=json.dumps(payload).encode(), headers=headers, method="POST"
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        body = response.read().decode()
        return response.headers.get("mcp-session-id"), body


def message(body):
    """The JSON-RPC message from a JSON or SSE response body."""
    for line in body.splitlines():
        if line.startswith("data:"):
            return json.loads(line[5:])
    return json.loads(body)


def gateway_models(url):
    session, _ = post(
        url,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "codex-voice", "version": "0"},
            },
        },
    )
    post(url, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
    _, body = post(
        url,
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "list_models", "arguments": {}},
        },
        session,
    )
    result = message(body)["result"]
    data = result.get("structuredContent") or json.loads(result["content"][0]["text"])
    return data.get("models", [])


def hf_cache():
    for var in ("HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"):
        if os.environ.get(var):
            return Path(os.environ[var])
    if os.environ.get("HF_HOME"):
        return Path(os.environ["HF_HOME"]) / "hub"
    return Path.home() / ".cache" / "huggingface" / "hub"


def kokoro_voices():
    voices = set()
    for snapshot in hf_cache().glob("models--*Kokoro*/snapshots/*/voices"):
        voices.update(p.stem for p in snapshot.iterdir() if p.suffix in (".safetensors", ".pt"))
    return sorted(voices)


def say_voices():
    try:
        out = subprocess.run(
            ["say", "-v", "?"], capture_output=True, text=True, timeout=10, check=True
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    return [line.split("  ")[0].strip() for line in out.splitlines() if line.strip()]


def main():
    url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:5051/mcp"
    try:
        models = gateway_models(url)
    except (OSError, urllib.error.URLError, ValueError, KeyError) as err:
        models = None
        print(f"local-models gateway ({url}): not reachable ({err})")
        print("  Start it in local-agents: uv run mcp_server.py --transport streamable-http\n")

    if models is not None:
        for kind, setting in (("speech", "tts_model"), ("transcription", "stt_model")):
            rows = [m for m in models if m.get("kind") == kind]
            label = "tts = local-models" if kind == "speech" else "stt = local-models"
            print(f"{kind} models ({label}, {setting} = <id>):")
            for m in rows:
                ready = "" if m.get("runtime_available") else "  (runtime missing)"
                print(f"  {m['id']}{ready}")
            if not rows:
                print("  none")
            print()

    voices = kokoro_voices()
    print("Kokoro voices cached (tts = kokoro or local-models, tts_voice = <name>):")
    print("  " + (", ".join(voices) if voices else "none"))
    print()
    names = say_voices()
    if names:
        print(f"macOS voices (tts = say, say_voice = <name>): {len(names)}, e.g.")
        print("  " + ", ".join(names[:12]))
        print("  All of them: say -v '?'")


if __name__ == "__main__":
    main()
