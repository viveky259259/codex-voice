# codex-local-voice

Local voice for codex-voice. Everything runs on the machine and no cloud
service is involved:

```
mic command (ffmpeg) → s16le 16 kHz → VAD → wav → local STT → Transcript event
speak(text) → local TTS → player            (mic muted while speaking + 400 ms)
```

- **Capture.** A command that writes raw s16le mono 16 kHz PCM to stdout. It
  runs as an argv array (never through a shell) in its own process group, and
  is killed with that group on shutdown or drop. If it dies, you get an
  `Error { stage: Capture }` event that ends with its stderr.
- **VAD.** `auto` (the default) uses the speech-detection model on macOS
  when `uv` is installed, and the energy detector otherwise (see
  "Model-based speech detection" below). `model`: a local model decides,
  frame by frame. It tells voice from hiss, fans and hum, so steady background
  noise never starts an utterance and steady speech is never cut off as
  noise. `energy`: energy over an adaptive noise floor (calibrated over the
  first 200 ms), with start and continue thresholds over the floor. Minimum
  speech length, end-of-utterance silence (700 ms), maximum length (30 s) and
  pre-roll (300 ms) apply to all of them.
- **Half duplex.** Mic audio is dropped while speaking and for
  `echo_tail_ms` afterwards. Barge-in is off by default. When it is on,
  sustained loud speech (24 dB over the floor for 300 ms) during playback
  stops the playback and starts an utterance.
- **STT backends.** `command` (whisper CLIs), `openai_compatible`
  (`/v1/audio/transcriptions`: whisper.cpp server, faster-whisper-server,
  LocalAI) and `mcp` (a job-based MCP server over streamable HTTP).
  Utterances are written as 16 kHz mono wav files to a private (0700) temp
  dir and deleted afterwards. Empty transcripts are dropped. So are noise
  transcripts: `[BLANK_AUDIO]`, `(silence)`, any text made only of
  `[...]`/`(...)` annotations, and "Thank you." under 0.6 s. The lists are
  configurable in `[transcripts]`.
- **TTS backends.** `command` (speaks by itself, e.g. `say`), `command_to_file`
  (writes `{wav}`, then the player plays it), `openai_compatible`
  (`/v1/audio/speech`, e.g. kokoro-fastapi) and `mcp`. Requests queue in
  order. `stop_speaking()` kills the player and drops the queue. Text is
  capped at `playback.max_chars` (400), cut at a sentence or word boundary.
  The text is always exactly one argv element, so quotes, `;` and `$(...)`
  reach the program unchanged.

## API

```rust
let (voice, mut events) = LocalVoice::start(config); // on a tokio runtime
voice.speak("Tests pass.");      // queued
voice.stop_speaking();           // kill the player, drop the queue
voice.set_muted(true);           // drop mic audio
voice.shutdown().await;          // kill and reap all children, remove temp dir

while let Some(event) = events.recv().await {
    match event {
        LocalVoiceEvent::Transcript { text } => { /* a user turn */ }
        LocalVoiceEvent::Error { stage, message } => { /* show it */ }
        _ => {} // Listening, SpeechStarted, SpeechEnded, SpeakingStarted, SpeakingFinished
    }
}
```

## Try it without the TUI

```bash
cargo run -p codex-local-voice -- config                      # defaults as TOML
cargo run -p codex-local-voice -- tools http://127.0.0.1:5051/mcp
cargo run -p codex-local-voice -- say --config voice.toml "Hello from Codex"
cargo run -p codex-local-voice -- listen --config voice.toml  # typed lines are spoken; /stop stops
```

`RUST_LOG=codex_local_voice=debug` logs the MCP tool schemas found at startup.

## macOS configurations

Commands use `{placeholders}`. Capture uses `{device}`. STT commands use
`{wav}`, `{out}` (a text file the command writes) and `{outdir}` (a directory
where the command writes a `.txt`). Without `{out}` or `{outdir}`, the
transcript is read from stdout. TTS uses `{text}` and `{wav}`. The player uses
`{wav}`. MCP argument templates use `{wav}`, `{text}` and `{job_id}`.

List the macOS audio inputs with
`ffmpeg -f avfoundation -list_devices true -i ""` and set `device` to an
index. The terminal app needs microphone permission (System Settings →
Privacy & Security → Microphone).

### 1. `say` + mlx_whisper (the macOS defaults)

```toml
[capture]
device = "0"   # ffmpeg -f avfoundation -i :0 -ac 1 -ar 16000 -f s16le -

[stt]
backend = "command"
command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-large-v3-turbo", "--language", "en",
           "--output-format", "txt", "--output-dir", "{outdir}", "--verbose", "False"]
timeout_ms = 60000

[tts]
backend = "command"
command = ["say", "{text}"]
```

The whisper.cpp CLI works too. It prints the text to stdout:
`command = ["whisper-cli", "-m", "/path/ggml-base.en.bin", "-f", "{wav}", "-nt"]`.

### 2. `say` to a file + whisper.cpp server

Start the server, serving the OpenAI path:
`whisper-server -m ggml-base.en.bin --port 8178 --inference-path /v1/audio/transcriptions`.
Alternatively, keep its default path and set `path = "/inference"`.

```toml
[stt]
backend = "openai_compatible"
base_url = "http://127.0.0.1:8178"
path = "/v1/audio/transcriptions"
model = "whisper-1"
language = "en"

[tts]
backend = "command_to_file"
command = ["say", "-o", "{wav}", "--data-format=LEI16@22050", "{text}"]

[playback]
player = ["afplay", "{wav}"]
```

For Kokoro through kokoro-fastapi (port 8880), use this `[tts]` instead:

```toml
[tts]
backend = "openai_compatible"
base_url = "http://127.0.0.1:8880"
model = "kokoro"
voice = "af_heart"
```

### 3. local-agents MCP for both

Start the server with `uv run mcp_server.py --transport streamable-http`, at
`http://127.0.0.1:5051/mcp`. Both directions use its `submit_inference` tool,
which returns a job that `get_job` polls. These names and model ids were
checked on a Mac against the real server (see issue #12). List the tools with
`cargo run -p codex-local-voice -- tools`, and the models with its
`list_models` tool.

```toml
[stt]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "submit_inference"
arguments = { model_id = "hf:mlx-community/whisper-large-v3-turbo", input_path = "{wav}", options = { language = "en" } }

[stt.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }
poll_interval_ms = 250
timeout_ms = 120000

[tts]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "submit_inference"
arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", prompt = "{text}", options = { voice = "af_heart", language = "en" } }

[tts.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }
poll_interval_ms = 250
timeout_ms = 120000

[playback]
player = ["afplay", "{wav}"]
```

Other transcription models: `hf:mlx-community/whisper-{tiny,small}-mlx` and
`hf:mlx-community/whisper-large-v3-turbo`. A short id like `whisper:base` is
rejected. The default result-path fallbacks find the transcript and the wav,
so `text_path` and `audio_path` aren't needed.

The MCP client sends `initialize`, `notifications/initialized` and
`tools/list` once, keeping the `Mcp-Session-Id` header. It then calls `tool`.
If the result has a job id (`job_id`, `jobId`, `job.id` or `id`; override
with `job.job_id_path`), it calls `job.tool` every `poll_interval_ms` until
the status (`status`/`state`, or `job.status_path`) is one of
`job.done_statuses` or `job.failed_statuses`. A result without a job id is
taken as final. The transcript is read from `text_path`, falling back to
`text`, `transcript`, `result.text` and similar fields. The audio is read from
`audio_path`, falling back to common path fields, then to any string ending
in `.wav`. The audio can be a file path, a `file://` URI or an `http(s)` URL,
which is downloaded. Responses can be JSON or SSE.

## Model-based speech detection

The model is the default on macOS. `[vad] backend = "auto"` (or no `[vad]`
at all) runs the bundled helper with `uv` when `uv` is installed, and the
energy detector otherwise. If the helper can't start (an offline first run,
or more than 60 s to get ready), `auto` logs a warning and uses the energy
detector. `backend = "model"` uses the model on any platform and fails
instead of falling back. `backend = "energy"` never starts the helper.

The helper returns a speech probability for each 32 ms frame. The bundled
one, [`helpers/silero_vad.py`](helpers/silero_vad.py), runs
[Silero VAD v5](https://github.com/snakers4/silero-vad) (MIT, 2.3 MB) on
onnxruntime. It is compiled into the binary and written to
`~/.cache/codex-voice/silero_vad.py` on first use, so an installed binary
needs no checkout. `uv run --script` installs onnxruntime and numpy in a
cached environment the first time. The model is downloaded once to
`~/.cache/codex-voice/silero_vad_v5.onnx` and its SHA-256 is checked
(`$CODEX_VOICE_MODELS` moves both; the helper also takes `--model PATH`).

```toml
[vad]
backend = "auto"       # or "model" / "energy"
end_silence_ms = 600   # a shorter tail than the default; the model doesn't need margin

[vad.model]
# command = ["my-vad-helper"]  # default: uv run --quiet --script <cache>/silero_vad.py
# start_probability = 0.5      # a frame at or above this may start speech
# continue_probability = 0.35  # speech continues while frames stay at or above this
# frame_samples = 512          # Silero v5 at 16 kHz
# startup_timeout_ms = 120000  # `auto` caps this at 60000 before falling back
```

In the TUI the same tables go under `[realtime.local.vad]` and
`[realtime.local.vad.model]`.

Any program can be the helper if it follows the protocol: read s16le mono
16 kHz PCM on stdin in frames of `frame_samples`, write `ready` once on
stdout, then one line per frame with a probability from 0.0 to 1.0. Barge-in
still compares against the energy noise floor, which keeps running beside the
model. With `backend = "model"`, a helper that fails to start stops
listening with an `Error { stage: Capture }` whose message starts with
`speech detection model:` and ends with the helper's stderr. A helper that
dies later does the same with any backend.

Measured on an M-series Mac: under 1 ms per frame, ready about 1.5 s after
start (once the environment is cached). White hiss at -28 dB scores at most
0.06, and speech about 1.0.

## Persistent Whisper server (Apple silicon)

The `mlx_whisper` command loads the model for every utterance, about 1.3 s
per transcript with `whisper-large-v3-turbo`.
[`helpers/whisper_server.py`](helpers/whisper_server.py) keeps it loaded and
serves the OpenAI transcription API on `127.0.0.1`, so the existing
`openai_compatible` backend uses it unchanged. Transcripts then arrive
0.2–0.4 s after the end of speech.

```bash
uv run --script codex-rs/local-voice/helpers/whisper_server.py   # --port 8178 --model mlx-community/whisper-large-v3-turbo
```

```toml
[stt]
backend = "openai_compatible"
base_url = "http://127.0.0.1:8178"
language = "en"
```

`GET /health` reports the loaded model. Whisper (turbo included) transcribes
pure noise as "Thank you." and turbo's `no_speech_prob` is always 0, so pair
the server with `[vad] backend = "model"`, which keeps noise from reaching
it.

## Other settings (defaults shown)

```toml
[vad]
frame_ms = 20
start_threshold_db = 12.0      # over the adaptive noise floor
continue_threshold_db = 6.0
min_speech_ms = 200
end_silence_ms = 700
max_utterance_ms = 30000
pre_roll_ms = 300
min_noise_floor = 60.0         # RMS on the i16 scale
noise_window_ms = 4000         # the floor follows a low percentile of this window,
noise_percentile = 0.1         #   in every state, so rising room noise ends utterances
steady_window_ms = 600         # 600 ms within steady_db is noise, not speech:
steady_db = 2.0                #   the floor moves up, a steady-from-the-start utterance is discarded
end_margin_db = 3.0            # end when the sound is back within 3 dB of the pre-speech background

[playback]
max_chars = 400
echo_tail_ms = 400
barge_in = false
barge_in_threshold_db = 24.0
barge_in_min_ms = 300

[transcripts]
drop = ["[BLANK_AUDIO]", "(silence)", "[silence]", "(blank audio)", "[no speech]",
        "[inaudible]", "[music]", "(music)"]
drop_if_short = ["Thank you.", "Thank you very much.", "Thank you so much.", "Thanks.",
                 "Thanks for watching!", "Thank you for watching.", "Please subscribe.",
                 "You", "Bye.", "Hmm.", "Um.", "Uh."]   # Whisper phantoms on noise
short_utterance_ms = 1500
drop_bracketed = true
max_distinct_word_ratio = 0.3  # drop loops like "it is it is it is ..." (>= repeat_min_words)
repeat_min_words = 8
```

On Linux the defaults are `ffmpeg -f pulse -i default`, `whisper-cli`,
`espeak-ng` and `aplay`.

## Tests

The tests need no audio hardware. They use synthetic PCM, fake capture, STT
and TTS scripts, and mock HTTP and MCP servers.

```bash
cargo test -p codex-local-voice
```
