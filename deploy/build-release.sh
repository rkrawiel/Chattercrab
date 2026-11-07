#!/usr/bin/env bash
# Builds Chattercrab release binaries into dist/.
#
# Usage: deploy/build-release.sh [--server-only] [--skip-linux-client] [--skip-windows-client]
#
# Targets:
#   chattercrab-signal  x86_64-unknown-linux-musl  -> dist/server/        (must be static)
#   chattercrab         x86_64-unknown-linux-gnu   -> dist/client-linux/
#   chattercrab         x86_64-pc-windows-gnu      -> dist/client-windows/
#
# Cross targets are built with `cargo zigbuild` when available, otherwise `cross`,
# otherwise plain `cargo` (with a warning). The host's own target always uses plain
# cargo. Set CHATTERCRAB_BUILDER=zigbuild|cross|cargo to force a builder.
# The Linux client needs the system libraries listed in docs/DEPLOY.md, so on a
# non-Linux host it is skipped unless CHATTERCRAB_FORCE_LINUX_CLIENT=1.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"

SERVER_TARGET=x86_64-unknown-linux-musl
LINUX_TARGET=x86_64-unknown-linux-gnu
WINDOWS_TARGET=x86_64-pc-windows-gnu

BUILD_LINUX_CLIENT=1
BUILD_WINDOWS_CLIENT=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --server-only)
            BUILD_LINUX_CLIENT=0
            BUILD_WINDOWS_CLIENT=0
            ;;
        --skip-linux-client) BUILD_LINUX_CLIENT=0 ;;
        --skip-windows-client) BUILD_WINDOWS_CLIENT=0 ;;
        -h | --help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "build-release.sh: unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift
done

log() { echo "==> $*"; }
warn() { echo "WARNING: $*" >&2; }
die() {
    echo "ERROR: $*" >&2
    exit 1
}

cd "$ROOT_DIR"

command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust with rustup"
HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"
log "host target: $HOST_TARGET"

if [[ "$HOST_TARGET" != *-linux-* ]] && ((BUILD_LINUX_CLIENT)) &&
    [[ "${CHATTERCRAB_FORCE_LINUX_CLIENT:-0}" != 1 ]]; then
    warn "skipping the Linux client on a non-Linux host (set CHATTERCRAB_FORCE_LINUX_CLIENT=1 to try)"
    BUILD_LINUX_CLIENT=0
fi

# ---------------------------------------------------------------------------
# Toolchain targets

wanted_targets=("$SERVER_TARGET")
((BUILD_LINUX_CLIENT)) && wanted_targets+=("$LINUX_TARGET")
((BUILD_WINDOWS_CLIENT)) && wanted_targets+=("$WINDOWS_TARGET")
if command -v rustup >/dev/null 2>&1; then
    installed="$(rustup target list --installed)"
    for t in "${wanted_targets[@]}"; do
        if ! grep -qx "$t" <<<"$installed"; then
            log "installing Rust target $t"
            rustup target add "$t"
        fi
    done
else
    warn "rustup not found; assuming targets ${wanted_targets[*]} are installed"
fi

# ---------------------------------------------------------------------------
# Builder selection

have_zig() {
    command -v zig >/dev/null 2>&1 && return 0
    # cargo-zigbuild also finds zig from the PyPI `ziglang` package.
    local py
    for py in python3 python; do
        if command -v "$py" >/dev/null 2>&1 && "$py" -c 'import ziglang' >/dev/null 2>&1; then
            return 0
        fi
    done
    return 1
}

CROSS_BUILDER="${CHATTERCRAB_BUILDER:-}"
if [[ -z "$CROSS_BUILDER" ]]; then
    if cargo zigbuild --help >/dev/null 2>&1 && have_zig; then
        CROSS_BUILDER=zigbuild
    elif command -v cross >/dev/null 2>&1; then
        CROSS_BUILDER=cross
    else
        CROSS_BUILDER=cargo
        warn "neither cargo-zigbuild (with zig) nor cross found; using plain cargo."
        warn "cross builds then need a C toolchain per target (musl-tools, mingw-w64)."
    fi
fi
log "cross builder: $CROSS_BUILDER"

# build <package> <binary> <target>
build() {
    local package="$1" bin="$2" target="$3"
    local -a cmd
    if [[ "$target" == "$HOST_TARGET" ]]; then
        cmd=(cargo build)
    else
        case "$CROSS_BUILDER" in
            zigbuild) cmd=(cargo zigbuild) ;;
            cross) cmd=(cross build) ;;
            cargo) cmd=(cargo build) ;;
            *) die "unknown CHATTERCRAB_BUILDER: $CROSS_BUILDER" ;;
        esac
    fi
    local -a locked=()
    [[ -f "$ROOT_DIR/Cargo.lock" ]] && locked=(--locked)
    log "building $bin for $target: ${cmd[*]}"
    "${cmd[@]}" --release "${locked[@]}" -p "$package" --bin "$bin" --target "$target"
}

# ---------------------------------------------------------------------------
# Static linking check (portable: reads the ELF program headers with od)

read_uint() { od -An -t "u$3" -j "$2" -N "$3" "$1" | tr -d ' \n'; }

# Succeeds if the ELF file has no PT_INTERP header, i.e. it needs no dynamic loader.
elf_is_static() {
    local file="$1"
    [[ "$(od -An -c -N 4 "$file" | tr -d ' ')" == '177ELF' ]] || return 1
    [[ "$(read_uint "$file" 4 1)" == 2 ]] || return 1 # ELFCLASS64
    local phoff phentsize phnum i type
    phoff="$(read_uint "$file" 32 8)"
    phentsize="$(read_uint "$file" 54 2)"
    phnum="$(read_uint "$file" 56 2)"
    for ((i = 0; i < phnum; i++)); do
        type="$(read_uint "$file" $((phoff + i * phentsize)) 4)"
        [[ "$type" == 3 ]] && return 1 # PT_INTERP
    done
    return 0
}

check_static() {
    local file="$1"
    if command -v file >/dev/null 2>&1; then
        log "file: $(file -b "$file")"
    fi
    if command -v ldd >/dev/null 2>&1 && [[ "$HOST_TARGET" == *-linux-* ]]; then
        log "ldd: $(ldd "$file" 2>&1 | head -n1 | sed 's/^[[:space:]]*//')"
    fi
    elf_is_static "$file" || die "$file is not statically linked (it has an ELF interpreter)"
    log "$file is statically linked"
}

# ---------------------------------------------------------------------------
# Build and collect

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR/server"

build chattercrab-server chattercrab-signal "$SERVER_TARGET"
SERVER_BIN="$ROOT_DIR/target/$SERVER_TARGET/release/chattercrab-signal"
check_static "$SERVER_BIN"
cp "$SERVER_BIN" "$DIST_DIR/server/"
chmod 0755 "$DIST_DIR/server/chattercrab-signal"

if ((BUILD_LINUX_CLIENT)); then
    build chattercrab-client chattercrab "$LINUX_TARGET"
    mkdir -p "$DIST_DIR/client-linux"
    cp "$ROOT_DIR/target/$LINUX_TARGET/release/chattercrab" "$DIST_DIR/client-linux/"
fi

if ((BUILD_WINDOWS_CLIENT)); then
    build chattercrab-client chattercrab "$WINDOWS_TARGET"
    mkdir -p "$DIST_DIR/client-windows"
    cp "$ROOT_DIR/target/$WINDOWS_TARGET/release/chattercrab.exe" "$DIST_DIR/client-windows/"
fi

# ---------------------------------------------------------------------------
# Checksums and sizes

cd "$DIST_DIR"
mapfile -t binaries < <(find . -type f \( -name chattercrab-signal -o -name chattercrab -o -name chattercrab.exe \) | sort)
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${binaries[@]}" >SHA256SUMS
else
    shasum -a 256 "${binaries[@]}" >SHA256SUMS
fi

log "artifacts:"
for b in "${binaries[@]}"; do
    size="$(wc -c <"$b" | tr -d ' ')"
    printf '    %-40s %10s bytes (%s KiB)\n' "${b#./}" "$size" "$((size / 1024))"
done
log "checksums written to dist/SHA256SUMS"
