#!/usr/bin/env bash
# bootstrap.sh — make a fresh environment ostrom-aware in one shot.
#
# Idempotently merges the ostrom marketplace + plugin into the USER-level
# settings (~/.claude/settings.json), so every repo in this environment
# gets the frozen rules + /ostrom:touch with no per-repo pointer. Also
# provisions a zero-secret default /ostrom:touch config.
#
# Use on:
#   - a new LOCAL machine
#   - a CLOUD / CI image
#
# Idempotent and safe to re-run; backs up an existing settings.json to
# settings.json.bak before writing.
#
# Usage:  ./bootstrap.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
POINTER="$SCRIPT_DIR/repo-pointer/settings.json"
CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
SETTINGS="$CONFIG_DIR/settings.json"

command -v jq >/dev/null 2>&1 || { echo "bootstrap: jq is required" >&2; exit 1; }
[ -f "$POINTER" ] || { echo "bootstrap: pointer not found at $POINTER" >&2; exit 1; }

# 1. Merge the ostrom keys into user-level settings, preserving everything else.
mkdir -p "$CONFIG_DIR"
if [ -f "$SETTINGS" ]; then
  merged="$(jq -s '.[0] * .[1]' "$SETTINGS" "$POINTER")"   # existing wins on scalars, ostrom keys added
else
  merged="$(cat "$POINTER")"
fi
tmp="$(mktemp)"
printf '%s\n' "$merged" > "$tmp"
[ -f "$SETTINGS" ] && cp "$SETTINGS" "$SETTINGS.bak"
mv "$tmp" "$SETTINGS"
echo "bootstrap: ostrom marketplace + plugin merged into $SETTINGS"

# 2. Provision a zero-secret default touch-log config (file provider) so
#    /ostrom:touch works immediately — no Notion account, nothing to copy. Never
#    overwrites an existing config, and never writes secrets.
OSTROM_DIR="$CONFIG_DIR/ostrom"
DEFAULTS="$SCRIPT_DIR/plugins/ostrom/config/touch-defaults.yaml"
mkdir -p "$OSTROM_DIR"
if [ -f "$OSTROM_DIR/config.yaml" ]; then
  echo "bootstrap: touch-log config already present at $OSTROM_DIR/config.yaml — left as-is"
elif [ -f "$DEFAULTS" ]; then
  cp "$DEFAULTS" "$OSTROM_DIR/config.yaml"
  echo "bootstrap: wrote default file-provider config to $OSTROM_DIR/config.yaml (zero secrets)"
else
  echo "bootstrap: NOTE touch-defaults.yaml not found at $DEFAULTS — skipped config provisioning" >&2
fi

echo "bootstrap: done. In Claude Code, run once per environment:  /plugin install ostrom@ostrom"
