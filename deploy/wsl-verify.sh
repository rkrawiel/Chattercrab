#!/usr/bin/env bash
# Full Linux verification of a Chattercrab checkout, for a Linux box or WSL.
#
# Usage (from Windows Git Bash, with MSYS path conversion disabled):
#   MSYS_NO_PATHCONV=1 wsl -d Ubuntu-24.04 -u root --exec bash \
#       /mnt/d/Chattercrab/deploy/wsl-verify.sh [--setup] [SOURCE_REPO]
#
#   --setup       install apt build dependencies, rustup, zig and cargo-zigbuild first
#   SOURCE_REPO   git repository to verify (default: the repository containing this
#                 script). It is cloned or fast-forwarded into ~/chattercrab-verify
#                 because building on /mnt/<drive> is slow.
#
# Steps: fmt, clippy, tests, release build of the workspace, deploy/build-release.sh,
# ShellCheck of the remaining deploy shell scripts, and an idle RSS measurement of
# the musl server.
# Exits non-zero on the first failing step.
set -euo pipefail

SETUP=0
SOURCE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --setup) SETUP=1 ;;
        -h | --help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) SOURCE="$1" ;;
    esac
    shift
done
SOURCE="${SOURCE:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
WORK="$HOME/chattercrab-verify"

step() { printf '\n==== %s\n' "$*"; }

if ((SETUP)); then
    step "installing build dependencies"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq build-essential cmake pkg-config git curl ca-certificates \
        musl-tools mingw-w64 nasm clang libclang-dev file shellcheck python3-pip \
        libasound2-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev \
        libxcb-xfixes0-dev libxrandr-dev libxkbcommon-dev libxkbcommon-x11-dev \
        libwayland-dev libpipewire-0.3-dev libdbus-1-dev libegl1-mesa-dev libgbm-dev
    if ! command -v rustup >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
        curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt
    fi
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
    python3 -c 'import ziglang' 2>/dev/null ||
        pip3 install --user --break-system-packages ziglang
    cargo zigbuild --help >/dev/null 2>&1 || cargo install --locked cargo-zigbuild
fi

# shellcheck source=/dev/null
[[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
export PATH="$HOME/.local/bin:$PATH"

git config --global --add safe.directory "$SOURCE" 2>/dev/null || true
if BRANCH="$(git -C "$SOURCE" rev-parse --abbrev-ref HEAD 2>/dev/null)"; then
    step "syncing $SOURCE ($BRANCH) into $WORK"
    if [[ -d "$WORK/.git" ]]; then
        git -C "$WORK" fetch -q "$SOURCE" "$BRANCH"
        git -C "$WORK" checkout -q -B "$BRANCH" FETCH_HEAD
    else
        rm -rf "$WORK"
        git clone -q --branch "$BRANCH" "$SOURCE" "$WORK"
    fi
else
    # Not usable as a git repository from here (e.g. a Windows git worktree whose
    # .git file points at a drive-letter path): copy the working tree instead.
    step "copying $SOURCE into $WORK (not a usable git repository)"
    mkdir -p "$WORK"
    find "$WORK" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -C "$SOURCE" --exclude=./target --exclude=./dist --exclude=./.git -cf - . | tar -C "$WORK" -xf -
fi
cd "$WORK"
git log --oneline -1 2>/dev/null || true
rustc --version

step "cargo fmt --check"
cargo fmt --all --check

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

step "cargo test --workspace"
cargo test --workspace

step "cargo build --release --workspace"
cargo build --release --workspace

step "deploy/build-release.sh"
bash deploy/build-release.sh

step "shellcheck"
shellcheck -S style deploy/*.sh

step "idle RSS of the musl server"
SERVER=dist/server/chattercrab-signal
PORT=18443
CHATTERCRAB_BIND="127.0.0.1:$PORT" RUST_LOG=warn "$SERVER" &
pid=$!
sleep 2
if kill -0 "$pid" 2>/dev/null; then
    curl -fsS "http://127.0.0.1:$PORT/health" && echo
    grep -E '^(VmRSS|VmHWM)' "/proc/$pid/status"
    start_ns="$(date +%s%N)"
    kill -TERM "$pid"
    wait "$pid" || true
    echo "SIGTERM to exit: $((($(date +%s%N) - start_ns) / 1000000)) ms"
else
    wait "$pid" || true
    echo "server exited immediately (stub binary?); RSS not measured"
fi

step "verification passed"
