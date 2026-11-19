#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10,<3.13"
# dependencies = [
#   "mlx-audio==0.5.9",
#   "misaki[en]==0.9.4",
#   "en-core-web-sm @ https://github.com/explosion/spacy-models/releases/download/en_core_web_sm-3.8.0/en_core_web_sm-3.8.0-py3-none-any.whl",
# ]
# ///
"""Persistent local text-to-speech server for codex-local-voice (Apple silicon).

Serves the OpenAI speech API on 127.0.0.1, so `[tts] backend =
"openai_compatible"` can use it:

    POST /v1/audio/speech   JSON: input, voice, speed (model is ignored)
    -> audio/wav (16-bit mono, 24 kHz)
    GET  /health            -> {"status": "ok", "model": "...", "voices": [...]}

Kokoro is loaded and warmed once at startup, instead of once per request as
with a job-based gateway, so a sentence takes well under a second.

The voice is a Kokoro voice name such as af_heart. Its first letter picks the
language (a: American English, b: British English, h: Hindi, e: Spanish, f:
French, i: Italian, j: Japanese, p: Portuguese, z: Mandarin); --lang
overrides it. Voices in the model's Hugging Face cache work offline; others
are downloaded once.

    uv run --script kokoro_server.py [--port 8880] [--model mlx-community/Kokoro-82M-8bit] [--voice af_heart]
"""

import argparse
import io
import json
import sys
import threading
import time
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import numpy as np
from huggingface_hub import try_to_load_from_cache
from mlx_audio.tts.utils import load

MAX_BODY = 1024 * 1024
LANGS = set("abefhijpz")


class Speaker:
    def __init__(self, model, voice, lang):
        self.name = model
        self.default_voice = voice
        self.lang = lang
        self.lock = threading.Lock()
        self.model = load(model)
        self.sample_rate = self.model.sample_rate
        # The model's cached snapshot (a partial one is fine), for its voices.
        if Path(model).is_dir():
            self.root = Path(model)
        else:
            config = try_to_load_from_cache(model, "config.json")
            self.root = Path(config).parent if isinstance(config, str) else None

    def voices(self):
        if self.root is None:
            return []
        return sorted(p.stem for p in (self.root / "voices").glob("*.safetensors"))

    def resolve(self, voice):
        voice = voice or self.default_voice
        if self.root is not None:
            path = self.root / "voices" / f"{voice}.safetensors"
            if path.is_file():
                return voice, str(path)
        return voice, voice

    def wav(self, text, voice=None, speed=1.0):
        name, voice = self.resolve(voice)
        lang = self.lang or (name[0] if name[:1] in LANGS else "a")
        with self.lock:
            chunks = [
                np.asarray(result.audio, dtype=np.float32).reshape(-1)
                for result in self.model.generate(
                    text=text, voice=voice, speed=speed, lang_code=lang
                )
            ]
        audio = np.concatenate(chunks) if chunks else np.zeros(0, dtype=np.float32)
        pcm = (np.clip(audio, -1.0, 1.0) * 32767).astype("<i2")
        out = io.BytesIO()
        with wave.open(out, "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(self.sample_rate)
            w.writeframes(pcm.tobytes())
        return out.getvalue(), len(pcm) / self.sample_rate

    def warm_up(self):
        started = time.monotonic()
        self.wav("Ready.")
        print(
            f"kokoro_server: {self.name} warm in {time.monotonic() - started:.2f}s",
            file=sys.stderr,
            flush=True,
        )


def handler_for(speaker):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, payload):
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/health":
                self.reply(200, {"status": "ok", "model": speaker.name, "voices": speaker.voices()})
            else:
                self.reply(404, {"error": {"message": "not found"}})

        def do_POST(self):
            if self.path.split("?")[0] != "/v1/audio/speech":
                self.reply(404, {"error": {"message": "not found"}})
                return
            length = int(self.headers.get("Content-Length") or 0)
            if length <= 0 or length > MAX_BODY:
                self.reply(400, {"error": {"message": "missing or oversized body"}})
                return
            try:
                request = json.loads(self.rfile.read(length))
                text = str(request.get("input") or "").strip()
                voice = request.get("voice") or None
                speed = float(request.get("speed") or 1.0)
            except (ValueError, TypeError) as err:
                self.reply(400, {"error": {"message": f"bad request: {err}"}})
                return
            if not text:
                self.reply(400, {"error": {"message": "input is empty"}})
                return
            fmt = request.get("response_format") or "wav"
            if fmt != "wav":
                self.reply(400, {"error": {"message": f"only wav is supported, not {fmt}"}})
                return
            started = time.monotonic()
            try:
                body, seconds = speaker.wav(text, voice, speed)
            except Exception as err:  # report, keep serving
                self.reply(500, {"error": {"message": str(err)}})
                return
            print(
                f"kokoro_server: {seconds:.1f}s of speech in {time.monotonic() - started:.2f}s",
                file=sys.stderr,
                flush=True,
            )
            self.send_response(200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8880)
    parser.add_argument("--model", default="mlx-community/Kokoro-82M-8bit")
    parser.add_argument("--voice", default="af_heart", help="default voice")
    parser.add_argument("--lang", default=None, help="Kokoro language code; default: from the voice")
    args = parser.parse_args()

    speaker = Speaker(args.model, args.voice, args.lang)
    speaker.warm_up()
    server = ThreadingHTTPServer((args.host, args.port), handler_for(speaker))
    print(
        f"kokoro_server: listening on http://{args.host}:{args.port}/v1/audio/speech",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
