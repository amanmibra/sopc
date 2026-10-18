#!/bin/sh
# Install sopc, the SOP compiler: one prebuilt binary from GitHub Releases. No Python or uv needed.
#
#   curl -fsSL https://raw.githubusercontent.com/amanmibra/sopc/main/install.sh | sh
#
# Options (environment variables):
#   SOPC_REF=<tag>              install a specific release, e.g. v0.0.15 (default: the latest release)
#   SOPC_INSTALL_DIR=<dir>      where to put the binary (default: ~/.local/bin)
#
# To build from a branch or commit instead, use Rust: cargo install --git https://github.com/amanmibra/sopc --branch <branch> (or --rev <commit>)

set -eu

REPO="amanmibra/sopc"
REF="${SOPC_REF:-latest}"
INSTALL_DIR="${SOPC_INSTALL_DIR:-$HOME/.local/bin}"
# For testing against local release archives (and checksums.txt): SOPC_RELEASES_URL=file:///path/to/dist
RELEASES_URL="${SOPC_RELEASES_URL:-}"

say() { printf 'sopc: %s\n' "$1"; }
fail() { printf 'sopc: error: %s\n' "$1" >&2; exit 1; }

case "$(uname -s)" in
  Linux) OS=linux ;;
  Darwin) OS=darwin ;;
  MINGW* | MSYS* | CYGWIN*) OS=windows ;;
  *) fail "unsupported operating system: $(uname -s). Download a binary from https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) ARCH=amd64 ;;
  arm64 | aarch64) ARCH=arm64 ;;
  *) fail "unsupported CPU architecture: $(uname -m). Download a binary from https://github.com/$REPO/releases" ;;
esac

EXT=tar.gz
EXE=
if [ "$OS" = windows ]; then
  EXT=zip
  EXE=.exe
fi
BIN="sopc$EXE"
ASSET="sopc_${OS}_${ARCH}.${EXT}"
# Releases before v0.0.6 (when the project was named OpenSOP) ship opensop_<os>_<arch> archives
# holding an `opensop` binary; it is installed as sopc.
LEGACY_ASSET="opensop_${OS}_${ARCH}.${EXT}"
SRC_BIN="$BIN"

case "$REF" in
  latest | v[0-9]*) ;;
  [0-9]*) REF="v$REF" ;;
  *) fail "SOPC_REF must be a release tag like v0.0.6 (got '$REF'). To build a branch or commit: cargo install --git https://github.com/$REPO --branch $REF (a commit: --rev $REF)" ;;
esac

if [ -n "$RELEASES_URL" ]; then
  BASE="$RELEASES_URL"
elif [ "$REF" = latest ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$REF"
fi

download() { # url dest
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    fail "need curl or wget to download sopc"
  fi
}

TMP="$(mktemp -d 2>/dev/null || mktemp -d -t sopc)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "downloading $ASSET ($REF)"
if ! download "$BASE/$ASSET" "$TMP/$ASSET" 2>/dev/null; then
  if download "$BASE/$LEGACY_ASSET" "$TMP/$LEGACY_ASSET" 2>/dev/null; then
    say "release $REF predates the rename; using $LEGACY_ASSET"
    ASSET="$LEGACY_ASSET"
    SRC_BIN="opensop$EXE"
  elif [ "$REF" = latest ]; then
    fail "no sopc release found at https://github.com/$REPO/releases (none published yet?). Build from source instead: cargo install --git https://github.com/$REPO"
  else
    fail "release $REF not found, or it has no $ASSET. See https://github.com/$REPO/releases for available versions"
  fi
fi

# Verify the checksum when the release has one and a sha256 tool is available.
if download "$BASE/checksums.txt" "$TMP/checksums.txt" 2>/dev/null; then
  EXPECTED="$(grep " $ASSET\$" "$TMP/checksums.txt" | cut -d ' ' -f 1 || true)"
  ACTUAL=""
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$TMP/$ASSET" | cut -d ' ' -f 1)"
  elif command -v shasum >/dev/null 2>&1; then
    ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | cut -d ' ' -f 1)"
  fi
  if [ -n "$EXPECTED" ] && [ -n "$ACTUAL" ] && [ "$EXPECTED" != "$ACTUAL" ]; then
    fail "checksum mismatch for $ASSET; try again, or download it by hand from https://github.com/$REPO/releases"
  fi
fi

mkdir -p "$TMP/x"
if [ "$EXT" = zip ]; then
  command -v unzip >/dev/null 2>&1 || fail "need unzip to extract $ASSET"
  unzip -q "$TMP/$ASSET" -d "$TMP/x"
else
  tar -xzf "$TMP/$ASSET" -C "$TMP/x"
fi
[ -f "$TMP/x/$SRC_BIN" ] || fail "$ASSET doesn't contain $SRC_BIN"

mkdir -p "$INSTALL_DIR"
cp "$TMP/x/$SRC_BIN" "$INSTALL_DIR/$BIN.tmp"
chmod 755 "$INSTALL_DIR/$BIN.tmp"
mv "$INSTALL_DIR/$BIN.tmp" "$INSTALL_DIR/$BIN"
say "installed $INSTALL_DIR/$BIN"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say "$INSTALL_DIR isn't on your PATH yet. Add it, then open a new terminal:"
    say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile"
    ;;
esac

say "next: 'sopc skills install' in your repo, then run the sopc-import skill in Claude Code, Codex or OpenCode"
