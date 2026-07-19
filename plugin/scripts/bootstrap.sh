#!/bin/sh
# forksan bootstrap: put the right forksan + forksan-daemon binaries into
# ${CLAUDE_PLUGIN_DATA}/bin. Prefers a prebuilt GitHub release, falls back
# to building from source with cargo. Never fails the calling hook.

set -u

REPO="TheUnderdev/forksan"
DATA="${CLAUDE_PLUGIN_DATA:-}"
ROOT="${CLAUDE_PLUGIN_ROOT:-}"
if [ -z "$DATA" ] || [ -z "$ROOT" ]; then
    echo "forksan bootstrap: CLAUDE_PLUGIN_DATA/CLAUDE_PLUGIN_ROOT unset" >&2
    exit 0
fi
mkdir -p "$DATA" 2>/dev/null

# Wanted version = the plugin's own version.
WANT=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$ROOT/.claude-plugin/plugin.json" | head -n1)
if [ -z "$WANT" ]; then
    echo "forksan bootstrap: cannot read plugin version" >&2
    exit 0
fi

HAVE=""
if [ -x "$DATA/bin/forksan" ]; then
    HAVE=$("$DATA/bin/forksan" --version 2>/dev/null | awk '{print $2}')
fi
if [ "$HAVE" = "$WANT" ]; then
    exit 0
fi
echo "forksan bootstrap: want v$WANT, have '${HAVE:-none}'"

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    Linux-x86_64) TARGET="x86_64-unknown-linux-musl" ;;
    Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-musl" ;;
    *) TARGET="" ;;
esac

TMP="$DATA/tmp"
rm -rf "$TMP"
mkdir -p "$TMP"

install_bins() {
    # $1 = dir holding new forksan + forksan-daemon
    mkdir -p "$DATA/bin.new"
    cp "$1/forksan" "$1/forksan-daemon" "$DATA/bin.new/" || return 1
    chmod +x "$DATA/bin.new/forksan" "$DATA/bin.new/forksan-daemon"
    rm -rf "$DATA/bin.old"
    if [ -d "$DATA/bin" ]; then
        mv "$DATA/bin" "$DATA/bin.old"
    fi
    mv "$DATA/bin.new" "$DATA/bin" || return 1
    rm -rf "$DATA/bin.old" "$TMP"
    echo "forksan bootstrap: installed v$WANT"
    # A running daemon keeps its old inode; the CLI's version handshake
    # retires it on the next session-start/stop.
    return 0
}

checksum_ok() {
    # $1 = file, $2 = .sha256 file (format: "<hex>  <name>")
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$TMP" && sha256sum -c "$(basename "$2")" >/dev/null 2>&1)
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$TMP" && shasum -a 256 -c "$(basename "$2")" >/dev/null 2>&1)
    else
        echo "forksan bootstrap: no sha256 tool; skipping verification" >&2
        return 0
    fi
}

# --- Attempt 1: prebuilt release artifact ---
if [ -n "$TARGET" ] && command -v curl >/dev/null 2>&1; then
    ASSET="forksan-v$WANT-$TARGET.tar.gz"
    URL="https://github.com/$REPO/releases/download/v$WANT/$ASSET"
    echo "forksan bootstrap: fetching $URL"
    if curl -fsSL --retry 2 -o "$TMP/$ASSET" "$URL" &&
        curl -fsSL --retry 2 -o "$TMP/$ASSET.sha256" "$URL.sha256"; then
        if checksum_ok "$TMP/$ASSET" "$TMP/$ASSET.sha256"; then
            if tar -xzf "$TMP/$ASSET" -C "$TMP" && install_bins "$TMP/bin"; then
                exit 0
            fi
        else
            echo "forksan bootstrap: checksum mismatch for $ASSET" >&2
        fi
    else
        echo "forksan bootstrap: download failed (no artifact for $TARGET?)" >&2
    fi
fi

# --- Attempt 2: build from source ---
if command -v cargo >/dev/null 2>&1 && command -v git >/dev/null 2>&1; then
    echo "forksan bootstrap: building v$WANT from source (this can take a few minutes)"
    SRC="$TMP/src"
    if git clone --quiet --depth 1 --branch "v$WANT" "https://github.com/$REPO.git" "$SRC" &&
        (cd "$SRC" && cargo build --quiet --release -p forksan -p forksan-daemon) &&
        install_bins "$SRC/target/release"; then
        exit 0
    fi
    echo "forksan bootstrap: source build failed" >&2
fi

echo "forksan bootstrap: could not install. Install manually with:" >&2
echo "  cargo install --git https://github.com/$REPO forksan forksan-daemon --root <dir>" >&2
echo "and place both binaries in $DATA/bin/" >&2
exit 0
