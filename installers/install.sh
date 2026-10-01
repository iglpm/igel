#!/bin/sh
# Installs the latest (or a given) release of Rune.
#
#   curl -fsSL https://runepm.com/install.sh | sh
#   curl -fsSL https://runepm.com/install.sh | sh -s -- --version v2027.0.0 --to ~/.local/bin
#
# Downloads the release archive for this machine from GitHub, checks it
# against its published SHA-256, and puts `rune` in ~/.rune/bin (or --to).
set -eu

REPO="runepm/rune"
VERSION="${RUNE_VERSION:-latest}"
DEST="${RUNE_INSTALL_DIR:-$HOME/.rune/bin}"

say() { printf '\033[1;35m▸\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --to) DEST="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,8p' "$0" 2>/dev/null || true
            exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

case "$(uname -s)" in
    Linux) os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *) die "no prebuilt Rune for $(uname -s); build it with: cargo install --git https://github.com/$REPO rune-cli" ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "no prebuilt Rune for $(uname -m)" ;;
esac
target="$arch-$os"
archive="rune-$target.tar.gz"

if [ -n "${RUNE_DOWNLOAD_BASE:-}" ]; then
    # A mirror holding the release files.
    base="$RUNE_DOWNLOAD_BASE"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "curl or wget is needed"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading Rune ($VERSION, $target)"
fetch "$base/$archive" "$tmp/$archive" || die "download failed: $base/$archive"
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || die "checksum download failed"

expected="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)"
else
    actual="$(shasum -a 256 "$tmp/$archive" | cut -d ' ' -f 1)"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected, got $actual"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$DEST"
install -m 755 "$tmp/rune-$target/rune" "$DEST/rune"
say "installed $("$DEST/rune" --version) to $DEST/rune"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        say "add it to your PATH, e.g.:"
        printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$DEST"
        ;;
esac
