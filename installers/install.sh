#!/bin/sh
# Installs the latest (or a given) release of Igel.
#
#   curl -fsSL https://runepm.com/install.sh | sh
#   curl -fsSL https://runepm.com/install.sh | sh -s -- --version 2027.0 --to ~/.local/bin
#
# Downloads the release archive for this machine from GitHub, checks it
# against its published SHA-256, and puts `igl` in ~/.igel/bin (or --to).
set -eu

REPO="runepm/igel"
VERSION="${IGEL_VERSION:-latest}"
DEST="${IGEL_INSTALL_DIR:-$HOME/.igel/bin}"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    say() { printf '\033[1;35m▸\033[0m %s\n' "$*"; }
else
    say() { printf '▸ %s\n' "$*"; }
fi
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
    die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
else
    die() { printf '✗ %s\n' "$*" >&2; exit 1; }
fi

usage() {
    cat <<'EOF'
Installs Igel.

  curl -fsSL https://runepm.com/install.sh | sh -s -- [options]

Options:
  --version <v>  a release, e.g. 2027.0 or v2027.0.1 (default: the latest)
  --to <dir>     where to put `igl` (default: ~/.igel/bin)
  -h, --help     this text

IGEL_VERSION and IGEL_INSTALL_DIR do the same; IGEL_DOWNLOAD_BASE points
at a mirror holding the release files.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version|--to)
            [ $# -ge 2 ] || die "$1 needs a value (see --help)"
            if [ "$1" = "--version" ]; then VERSION="$2"; else DEST="$2"; fi
            shift 2 ;;
        --version=*) VERSION="${1#*=}"; shift ;;
        --to=*) DEST="${1#*=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

# `2027.0`, `v2027.0` and `v2027.0.0` all name release tag `v2027.0.0`.
if [ "$VERSION" != "latest" ]; then
    v="${VERSION#v}"
    case "$v" in
        *[!0-9.]*|.*|*.|*..*|"") die "not a release version: $VERSION (e.g. 2027.0 or 2027.0.1)" ;;
    esac
    case "$v" in
        *.*.*) ;;
        *.*) v="$v.0" ;;
        *) v="$v.0.0" ;;
    esac
    VERSION="v$v"
fi

case "$(uname -s)" in
    Linux)
        os="unknown-linux-gnu"
        # The Linux builds link glibc dynamically.
        if ldd --version 2>&1 | grep -qi musl; then
            die "no prebuilt Igel for musl-based Linux (e.g. Alpine) yet; build it with: cargo install --locked --git https://github.com/$REPO igel-cli"
        fi
        ;;
    Darwin) os="apple-darwin" ;;
    MINGW*|MSYS*|CYGWIN*)
        die "on Windows, install from PowerShell: irm https://runepm.com/install.ps1 | iex" ;;
    *) die "no prebuilt Igel for $(uname -s); build it with: cargo install --locked --git https://github.com/$REPO igel-cli" ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "no prebuilt Igel for $(uname -m); build it with: cargo install --locked --git https://github.com/$REPO igel-cli" ;;
esac
# A shell running under Rosetta on Apple silicon reports x86_64: install the
# native build.
if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ] \
    && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
    arch="aarch64"
fi
target="$arch-$os"
archive="igel-$target.tar.gz"

if [ -n "${IGEL_DOWNLOAD_BASE:-}" ]; then
    # A mirror holding the release files.
    base="${IGEL_DOWNLOAD_BASE%/}"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

# HTTPS only, redirects included (a mirror may be plain HTTP or a file://).
proto="=https"
[ -z "${IGEL_DOWNLOAD_BASE:-}" ] || proto="all"
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --proto "$proto" --proto-redir "$proto" --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "curl or wget is needed"
fi
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    die "sha256sum or shasum is needed to check the download"
fi
command -v tar >/dev/null 2>&1 || die "tar is needed"

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t igel-install)"
trap 'rm -rf "$tmp"' EXIT

say "downloading Igel ($VERSION, $target)"
if ! fetch "$base/$archive" "$tmp/$archive"; then
    if [ "$VERSION" = "latest" ] || [ -n "${IGEL_DOWNLOAD_BASE:-}" ]; then
        die "download failed: $base/$archive"
    fi
    die "download failed: $base/$archive (is $VERSION a release? see https://github.com/$REPO/releases)"
fi
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || die "checksum download failed: $base/$archive.sha256"

expected="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
actual="$(sha256 "$tmp/$archive")"
[ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected, got $actual"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$DEST" || die "cannot create $DEST (pick another place with --to)"
# Replace the file rather than overwrite it in place: a running `igl` keeps
# its old copy.
cp "$tmp/igel-$target/igl" "$DEST/.igel.new" || die "cannot write to $DEST (pick another place with --to)"
chmod 755 "$DEST/.igel.new"
mv -f "$DEST/.igel.new" "$DEST/igl"
say "installed $("$DEST/igl" --version) to $DEST/igl"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        say "add it to your PATH:"
        case "${SHELL:-}" in
            */fish) printf '    fish_add_path %s\n' "$DEST" ;;
            */zsh) printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc\n' "$DEST" ;;
            */bash) printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.bashrc\n' "$DEST" ;;
            *) printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$DEST" ;;
        esac
        say "then open a new terminal, or run: export PATH=\"$DEST:\$PATH\""
        ;;
esac
