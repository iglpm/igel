#!/bin/sh
# Uninstalls Rune.
#
#   curl -fsSL https://runepm.com/uninstall.sh | sh
#   curl -fsSL https://runepm.com/uninstall.sh | sh -s -- --from ~/.local/bin --purge
#
# Removes the `rune` binary from ~/.rune/bin (or --from). Pass --purge to also
# remove Rune's cache and configuration directory (~/.rune).
set -eu

DEST="${RUNE_INSTALL_DIR:-$HOME/.rune/bin}"
RUNE_HOME="${RUNE_HOME:-$HOME/.rune}"
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
    printf 'Are you sure you want to uninstall Rune from %s? [y/N] ' "$DEST"
    read -r answer
    case "$answer" in
        [yY]|[yY][eE][sS]) ;;
        *) say "uninstallation cancelled"; exit 0 ;;
    esac
fi

say "uninstalling Rune..."

bin_removed=0
if [ -f "$DEST/rune" ]; then
    rm -f "$DEST/rune"
    success "removed $DEST/rune"
    bin_removed=1
else
    say "binary not found at $DEST/rune"
fi

# Clean up empty bin directory if within ~/.rune
if [ -d "$DEST" ] && [ "$DEST" = "$RUNE_HOME/bin" ]; then
    rmdir "$DEST" 2>/dev/null || true
fi

if [ "$PURGE" -eq 1 ]; then
    if [ -d "$RUNE_HOME" ]; then
        rm -rf "$RUNE_HOME"
        success "removed $RUNE_HOME (cache and configuration)"
    fi
else
    if [ -d "$RUNE_HOME" ]; then
        say "configuration and cache preserved at $RUNE_HOME (use --purge to delete)"
    fi
fi

# PATH guidance
case ":$PATH:" in
    *":$DEST:"*)
        say "note: $DEST is still in your PATH"
        say "remember to remove it from your shell configuration (e.g. ~/.profile, ~/.bashrc, ~/.zshrc)"
        ;;
esac

success "Rune uninstalled successfully"
