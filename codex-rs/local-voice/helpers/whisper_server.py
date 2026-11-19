#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["mlx-whisper>=0.4"]
# ///
"""Persistent local speech-to-text server for codex-local-voice (Apple silicon).

Serves the OpenAI transcription API on 127.0.0.1, so
`[stt] backend = "openai_compatible"` can use it:

    POST /v1/audio/transcriptions   multipart: file, model, language, response_format
    -> {"text": "..."}
    GET  /health                    -> {"status": "ok", "model": "..."}

The Whisper model is loaded and warmed once at startup, instead of once per
utterance as with the `mlx_whisper` command. Segments Whisper itself rates as
probably not speech (high no_speech_prob with a low average log-probability)
are dropped. whisper-large-v3-turbo reports no_speech_prob 0.0 even for pure
noise (and transcribes it as "Thank you."), so with turbo this guard does
nothing: keep noise away from Whisper with `[vad] backend = "model"`.

    uv run --script whisper_server.py [--port 8178] [--model mlx-community/whisper-large-v3-turbo]
"""

import argparse
import email.parser
import email.policy
import json
import os
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import mlx_whisper
import numpy as np

MAX_BODY = 50 * 1024 * 1024


def parse_multipart(content_type, body):
    """Returns ({field: str}, file_bytes or None) for a multipart/form-data body."""
    message = email.parser.BytesParser(policy=email.policy.HTTP).parsebytes(
        b"Content-Type: " + content_type.encode("latin-1") + b"\r\n\r\n" + body
    )
    fields, audio = {}, None
    for part in message.iter_parts():
        name = part.get_param("name", header="content-disposition")
        if name is None:
            continue
        payload = part.get_payload(decode=True) or b""
        if part.get_filename() is not None or name == "file":
            audio = payload
        else:
            fields[name] = payload.decode("utf-8", "replace")
    return fields, audio


class Transcriber:
    def __init__(self, model, language, no_speech_threshold, logprob_threshold):
        self.model = model
        self.language = language
        self.no_speech_threshold = no_speech_threshold
        self.logprob_threshold = logprob_threshold
        # MLX evaluation isn't meant to run concurrently on one model.
        self.lock = threading.Lock()

    def warm_up(self):
        started = time.monotonic()
        self.transcribe_array(np.zeros(16000, dtype=np.float32), self.language)
        print(
            f"whisper_server: {self.model} loaded in {time.monotonic() - started:.1f}s",
            file=sys.stderr,
            flush=True,
        )

    def transcribe_array(self, audio, language):
        options = {"path_or_hf_repo": self.model, "verbose": None}
        if language:
            options["language"] = language
        with self.lock:
            return mlx_whisper.transcribe(audio, **options)

    def transcribe_file(self, data, language):
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as handle:
            handle.write(data)
            path = handle.name
        try:
            options = {"path_or_hf_repo": self.model, "verbose": None}
            if language:
                options["language"] = language
            with self.lock:
                result = mlx_whisper.transcribe(path, **options)
        finally:
            os.unlink(path)
        kept = [
            segment
            for segment in result.get("segments", [])
            if not (
                segment.get("no_speech_prob", 0.0) > self.no_speech_threshold
                and segment.get("avg_logprob", 0.0) < self.logprob_threshold
            )
        ]
        return " ".join(segment.get("text", "").strip() for segment in kept).strip()


def handler_for(transcriber):
    class Handler(BaseHTTPRequestHandler):
        server_version = "codex-voice-whisper/1"

        def log_message(self, fmt, *args):
            print("whisper_server: " + fmt % args, file=sys.stderr, flush=True)

        def reply(self, status, payload):
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path.split("?")[0] == "/health":
                self.reply(200, {"status": "ok", "model": transcriber.model})
            else:
                self.reply(404, {"error": {"message": "not found"}})

        def do_POST(self):
            if self.path.split("?")[0] not in ("/v1/audio/transcriptions", "/inference"):
                self.reply(404, {"error": {"message": "not found"}})
                return
            length = int(self.headers.get("Content-Length") or 0)
            if length <= 0 or length > MAX_BODY:
                self.reply(413 if length > MAX_BODY else 400, {"error": {"message": "bad length"}})
                return
            content_type = self.headers.get("Content-Type", "")
            if not content_type.startswith("multipart/form-data"):
                self.reply(400, {"error": {"message": "expected multipart/form-data"}})
                return
            fields, audio = parse_multipart(content_type, self.rfile.read(length))
            if not audio:
                self.reply(400, {"error": {"message": "missing file"}})
                return
            started = time.monotonic()
            try:
                text = transcriber.transcribe_file(
                    audio, fields.get("language") or transcriber.language
                )
            except Exception as err:  # report, keep serving
                self.reply(500, {"error": {"message": str(err)}})
                return
            print(
                f"whisper_server: transcribed {len(audio)} bytes in "
                f"{time.monotonic() - started:.2f}s",
                file=sys.stderr,
                flush=True,
            )
            if fields.get("response_format") == "text":
                body = text.encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.reply(200, {"text": text})

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8178)
    parser.add_argument("--model", default="mlx-community/whisper-large-v3-turbo")
    parser.add_argument("--language", default=None, help="e.g. en; default: detect")
    parser.add_argument("--no-speech-threshold", type=float, default=0.6)
    parser.add_argument("--logprob-threshold", type=float, default=-1.0)
    args = parser.parse_args()

    transcriber = Transcriber(
        args.model, args.language, args.no_speech_threshold, args.logprob_threshold
    )
    transcriber.warm_up()
    server = ThreadingHTTPServer((args.host, args.port), handler_for(transcriber))
    print(
        f"whisper_server: listening on http://{args.host}:{args.port}/v1/audio/transcriptions",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
