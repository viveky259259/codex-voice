#!/bin/sh
# Build and install codex-voice from this checkout (macOS).
#
#   scripts/codex-voice/install.sh               release build (about 20 min), then install
#   scripts/codex-voice/install.sh --skip-build  install the binaries already in codex-rs/target/release
#
# Installs into CODEX_VOICE_HOME (default ~/.codex-voice): bin/codex,
# bin/codex-local-voice, helpers/, log/, config.toml and voice.conf (both kept
# if they exist), and the codex-voice launcher into CODEX_VOICE_BIN_DIR
# (default ~/.local/bin). ~/.codex is not touched.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$REPO/scripts/codex-voice"
CV="${CODEX_VOICE_HOME:-$HOME/.codex-voice}"
BIN_DIR="${CODEX_VOICE_BIN_DIR:-$HOME/.local/bin}"
RELEASE="$REPO/codex-rs/target/release"

build=1
for arg in "$@"; do
  case "$arg" in
    --skip-build) build=0 ;;
    -h | --help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "install.sh: unknown option $arg" >&2
      exit 2
      ;;
  esac
done

# shellcheck disable=SC1003 # a literal backslash
case "$CV" in
  *'"'* | *'\'*)
    echo "install.sh: CODEX_VOICE_HOME can't contain quotes or backslashes" >&2
    exit 1
    ;;
esac

PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$HOME/.cargo/bin"

if [ "$build" = 1 ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: cargo is missing; install Rust from https://rustup.rs" >&2
    exit 1
  fi
  echo "Building codex and codex-local-voice (release; about 20 minutes the first time)..."
  # libgit2 can't fetch one of v8's submodules; the git CLI can.
  (cd "$REPO/codex-rs" && CARGO_NET_GIT_FETCH_WITH_CLI=true \
    cargo build --release -p codex-cli --bin codex -p codex-local-voice --bin codex-local-voice)
fi

for b in codex codex-local-voice; do
  if [ ! -x "$RELEASE/$b" ]; then
    echo "install.sh: $RELEASE/$b is missing; run without --skip-build" >&2
    exit 1
  fi
done

mkdir -p "$CV/bin" "$CV/helpers" "$CV/log" "$BIN_DIR"

# Copy, then rename: replacing a running binary in place can crash it.
install_file() {
  cp "$1" "$2.new"
  chmod "$3" "$2.new"
  mv -f "$2.new" "$2"
}

install_file "$RELEASE/codex" "$CV/bin/codex" 755
install_file "$RELEASE/codex-local-voice" "$CV/bin/codex-local-voice" 755
install_file "$REPO/codex-rs/local-voice/helpers/whisper_server.py" "$CV/helpers/whisper_server.py" 644
install_file "$REPO/codex-rs/local-voice/helpers/silero_vad.py" "$CV/helpers/silero_vad.py" 644
install_file "$HERE/voice_models.py" "$CV/helpers/voice_models.py" 644
install_file "$HERE/codex-voice" "$BIN_DIR/codex-voice" 755

if [ -f "$CV/config.toml" ]; then
  echo "Kept $CV/config.toml"
else
  sed "s|@CODEX_VOICE_HOME@|$CV|" "$HERE/config.toml" >"$CV/config.toml"
  chmod 600 "$CV/config.toml"
  echo "Wrote $CV/config.toml"
fi
if [ -f "$CV/voice.conf" ]; then
  echo "Kept $CV/voice.conf"
else
  install_file "$HERE/voice.conf" "$CV/voice.conf" 644
  echo "Wrote $CV/voice.conf"
fi

echo "Installed codex-voice: $BIN_DIR/codex-voice"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on PATH; add it to your shell profile." ;;
esac
for tool in ffmpeg uv claude; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    case "$tool" in
      ffmpeg) hint="brew install ffmpeg (microphone capture)" ;;
      uv) hint="brew install uv (Whisper server and speech detection)" ;;
      claude) hint="install Claude Code and sign in (the default agent)" ;;
    esac
    echo "Missing: $tool. $hint"
  fi
done
echo "Give your terminal microphone access: System Settings > Privacy & Security > Microphone."
echo "Pick voice models in $CV/voice.conf; 'codex-voice models' lists them."
