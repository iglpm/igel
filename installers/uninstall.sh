#!/bin/sh
# Uninstalls Igel.
#
#   curl -fsSL https://runepm.com/uninstall.sh | sh
#   curl -fsSL https://runepm.com/uninstall.sh | sh -s -- --from ~/.local/bin --purge
#
# Removes the `igl` binary from ~/.igel/bin (or --from). Pass --purge to also
# remove Igel's cache and configuration directory (~/.igel).
set -eu

DEST="${IGEL_INSTALL_DIR:-$HOME/.igel/bin}"
IGEL_HOME="${IGEL_HOME:-$HOME/.igel}"
PURGE=0
ASSUME_YES=0

say() { printf '\033[1;35m▸\033[0m %s\n' "$*"; }
success() { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --from|--to) DEST="$2"; shift 2 ;;
        --purge|--all) PURGE=1; shift ;;
        -y|--yes) ASSUME_YES=1; shift ;;
        -h|--help)
            sed -n '2,8p' "$0" 2>/dev/null || true
            exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

if [ "$ASSUME_YES" -eq 0 ] && [ -t 0 ]; then
    printf 'Are you sure you want to uninstall Igel from %s? [y/N] ' "$DEST"
    read -r answer
    case "$answer" in
        [yY]|[yY][eE][sS]) ;;
        *) say "uninstallation cancelled"; exit 0 ;;
    esac
fi

say "uninstalling Igel..."

bin_removed=0
if [ -f "$DEST/igl" ]; then
    rm -f "$DEST/igl"
    success "removed $DEST/igl"
    bin_removed=1
else
    say "binary not found at $DEST/igl"
fi

# Clean up empty bin directory if within ~/.igel
if [ -d "$DEST" ] && [ "$DEST" = "$IGEL_HOME/bin" ]; then
    rmdir "$DEST" 2>/dev/null || true
fi

if [ "$PURGE" -eq 1 ]; then
    if [ -d "$IGEL_HOME" ]; then
        rm -rf "$IGEL_HOME"
        success "removed $IGEL_HOME (cache and configuration)"
    fi
else
    if [ -d "$IGEL_HOME" ]; then
        say "configuration and cache preserved at $IGEL_HOME (use --purge to delete)"
    fi
fi

# PATH guidance
case ":$PATH:" in
    *":$DEST:"*)
        say "note: $DEST is still in your PATH"
        say "remember to remove it from your shell configuration (e.g. ~/.profile, ~/.bashrc, ~/.zshrc)"
        ;;
esac

success "Igel uninstalled successfully"
