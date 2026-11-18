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
- **VAD.** Energy based, with an adaptive noise floor (calibrated over the
  first 200 ms). Settings: start and continue thresholds over the floor,
  minimum speech length, end-of-utterance silence (700 ms), maximum length
  (30 s) and pre-roll (300 ms).
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
command = ["mlx_whisper", "{wav}", "--model", "mlx-community/whisper-base-mlx",
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
`http://127.0.0.1:5051/mcp`. **The tool names and argument names below are
guesses.** Check them first with `cargo run -p codex-local-voice -- tools`.
A wrong name produces an error that lists the server's tools.

```toml
[stt]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "transcribe"
arguments = { model_id = "whisper:base", input_path = "{wav}" }
# text_path = "result.text"      # where the transcript is in the finished job

[stt.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }
poll_interval_ms = 250
timeout_ms = 120000

[tts]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "speak"
arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", voice = "af_heart", text = "{text}" }
# audio_path = "artifacts.0.path"  # where speech.wav is in the finished job
# artifact_dir = "/path/to/local-agents/artifacts"   # for relative paths

[tts.job]
tool = "get_job"
arguments = { job_id = "{job_id}" }

[playback]
player = ["afplay", "{wav}"]
```

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

[playback]
max_chars = 400
echo_tail_ms = 400
barge_in = false
barge_in_threshold_db = 24.0
barge_in_min_ms = 300

[transcripts]
drop = ["[BLANK_AUDIO]", "(silence)", "[silence]", "(blank audio)", "[no speech]",
        "[inaudible]", "[music]", "(music)"]
drop_if_short = ["Thank you.", "Thanks.", "Thanks for watching!",
                 "Thank you for watching.", "You", "Bye."]
short_utterance_ms = 600
drop_bracketed = true
```

On Linux the defaults are `ffmpeg -f pulse -i default`, `whisper-cli`,
`espeak-ng` and `aplay`.

## Tests

The tests need no audio hardware. They use synthetic PCM, fake capture, STT
and TTS scripts, and mock HTTP and MCP servers.

```bash
cargo test -p codex-local-voice
```
