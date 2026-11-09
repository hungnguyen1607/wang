#!/bin/sh
# wang installer:  curl -fsSL <url>/install.sh | sh
# Downloads the release for this machine, checks its SHA-256, installs to ~/.local, no sudo.
# Overrides: WANG_RELEASES (download base URL), WANG_HOME (install dir), WANG_BIN (link dir).
set -eu

RELEASES="${WANG_RELEASES:-https://github.com/hungnguyen1607/wang/releases/latest/download}"
DEST="${WANG_HOME:-$HOME/.local/lib/wang}"
BIN="${WANG_BIN:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'wang install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "wang runs on Linux only for now (its sandbox uses Linux namespaces)."
arch="$(uname -m)"
case "$arch" in
  x86_64|aarch64) ;;
  *) fail "no build for $arch yet." ;;
esac
name="wang-${arch}-linux"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  fail "needs curl or wget."
fi
command -v sha256sum >/dev/null 2>&1 || fail "needs sha256sum (coreutils)."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
say "Downloading $name..."
fetch "$RELEASES/$name.tar.gz" "$tmp/$name.tar.gz" || fail "download failed: $RELEASES/$name.tar.gz"
fetch "$RELEASES/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || fail "checksum download failed"
(cd "$tmp" && sha256sum -c "$name.tar.gz.sha256" >/dev/null 2>&1) || fail "checksum mismatch: not installing."

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
mkdir -p "$DEST" "$BIN"
for f in wang wang-proxy jail wang-vm-agent VERSION README.md; do
  [ -e "$tmp/$name/$f" ] && cp "$tmp/$name/$f" "$DEST/$f"
done
chmod 755 "$DEST/wang" "$DEST/wang-proxy" "$DEST/jail" "$DEST/wang-vm-agent"
ln -sf "$DEST/wang" "$BIN/wang"
ln -sf "$DEST/wang-proxy" "$BIN/wang-proxy"
say "Installed wang $(cat "$DEST/VERSION" 2>/dev/null || echo '?') to $DEST"

command -v bwrap >/dev/null 2>&1 || say "Note: wang's sandbox needs bubblewrap:  sudo apt install bubblewrap"
case ":$PATH:" in
  *":$BIN:"*) say "Next: run  wang" ;;
  *) say "Next: add $BIN to your PATH (e.g. echo 'export PATH=\"$BIN:\$PATH\"' >> ~/.bashrc), then run  wang" ;;
esac
