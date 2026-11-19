#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["onnxruntime>=1.18", "numpy>=1.26"]
# ///
"""Speech-probability helper for codex-local-voice (`[vad] backend = "model"`).

Reads raw s16le mono 16 kHz PCM on stdin in frames of 512 samples (32 ms) and
writes one line per frame on stdout: the probability (0.000-1.000) that the
frame contains speech. It prints `ready` once the model is loaded.

The model is Silero VAD v5 (MIT, https://github.com/snakers4/silero-vad). It
is downloaded once to $CODEX_VOICE_MODELS (default ~/.cache/codex-voice) or
read from --model PATH.

Run it with `uv run --script silero_vad.py`, which installs onnxruntime and
numpy in a cached environment, or with any python that has both.
"""

import argparse
import hashlib
import os
import sys
import urllib.request

import numpy as np
import onnxruntime as ort

SAMPLE_RATE = 16000
FRAME = 512  # samples per frame at 16 kHz (Silero v5 requirement)
CONTEXT = 64  # samples of the previous frame prepended to each frame
MODEL_URL = (
    "https://github.com/snakers4/silero-vad/raw/v5.1.2/src/silero_vad/data/silero_vad.onnx"
)
MODEL_SHA256 = "2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f"


def model_path(explicit):
    if explicit:
        return explicit
    root = os.environ.get("CODEX_VOICE_MODELS") or os.path.join(
        os.path.expanduser("~"), ".cache", "codex-voice"
    )
    path = os.path.join(root, "silero_vad_v5.onnx")
    if not os.path.exists(path):
        os.makedirs(root, exist_ok=True)
        print(f"silero_vad: downloading {MODEL_URL}", file=sys.stderr, flush=True)
        tmp = path + ".part"
        with urllib.request.urlopen(MODEL_URL, timeout=60) as response, open(tmp, "wb") as out:
            out.write(response.read())
        digest = hashlib.sha256(open(tmp, "rb").read()).hexdigest()
        if digest != MODEL_SHA256:
            os.remove(tmp)
            raise SystemExit(f"silero_vad: unexpected model checksum {digest}")
        os.replace(tmp, path)
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--model", help="path to silero_vad.onnx (v5)")
    args = parser.parse_args()

    options = ort.SessionOptions()
    options.intra_op_num_threads = 1
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(
        model_path(args.model), sess_options=options, providers=["CPUExecutionProvider"]
    )
    state = np.zeros((2, 1, 128), dtype=np.float32)
    context = np.zeros((1, CONTEXT), dtype=np.float32)
    sr = np.array(SAMPLE_RATE, dtype=np.int64)

    stdin = sys.stdin.buffer
    stdout = sys.stdout
    stdout.write("ready\n")
    stdout.flush()
    need = FRAME * 2
    while True:
        data = stdin.read(need)
        if not data or len(data) < need:
            break
        frame = np.frombuffer(data, dtype="<i2").astype(np.float32)[None, :] / 32768.0
        x = np.concatenate([context, frame], axis=1)
        prob, state = session.run(None, {"input": x, "state": state, "sr": sr})
        context = frame[:, -CONTEXT:]
        stdout.write(f"{float(prob[0][0]):.3f}\n")
        stdout.flush()


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, KeyboardInterrupt):
        pass
