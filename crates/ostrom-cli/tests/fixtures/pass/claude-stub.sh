#!/usr/bin/env bash
set -eu

OSTROM_HOME="$CLAUDE_CONFIG_DIR/ostrom" "$OSTROM_FIXTURE_BIN" trace append pass-started \
  '{"owner":"builder-placeholder-session-wake7"}' '{}' >/dev/null
printf '%s\n' '{"type":"result","total_cost_usd":1.25}'
