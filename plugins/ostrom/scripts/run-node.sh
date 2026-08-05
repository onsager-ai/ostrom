#!/usr/bin/env bash
# run-node.sh — find Node.js before running the bundled /ostrom:doctor tool.
#
# WHY THIS SHIM EXISTS:
# nvm is normally loaded by an interactive shell profile. Claude Code hooks
# and plugin scripts run non-interactively, so a perfectly healthy nvm-managed
# `node` often is not on PATH. /ostrom:doctor is on-demand tooling and may rely on
# Node, but it must resolve the user's installed runtime without sourcing an
# interactive profile or changing the machine.
#
# Resolution is first-hit-wins: PATH, nvm's default alias, fnm, volta, asdf,
# then conventional standalone install locations.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

run_if_node() {
  [ -x "$1" ] || return 1
  node_path="$1"
  shift
  exec "$node_path" "$PLUGIN_ROOT/dist/doctor.js" "$@"
}

if command -v node >/dev/null 2>&1; then
  exec node "$PLUGIN_ROOT/dist/doctor.js" "$@"
fi

nvm_dir="${NVM_DIR:-$HOME/.nvm}"
nvm_alias="$nvm_dir/alias/default"
if [ -f "$nvm_alias" ]; then
  default="$(sed -n '1{s/[[:space:]]//g;p;}' "$nvm_alias")"
  case "$default" in
    v[0-9]*.[0-9]*.[0-9]*|[0-9]*.[0-9]*.[0-9]*)
      version="${default#v}"
      run_if_node "$nvm_dir/versions/node/v$version/bin/node" "$@"
      ;;
    v[0-9]*|[0-9]*)
      major="${default#v}"
      best_path=""
      best_minor=-1
      best_patch=-1
      for candidate in "$nvm_dir/versions/node/v$major".*/bin/node; do
        [ -x "$candidate" ] || continue
        version="${candidate#"$nvm_dir/versions/node/v"}"
        version="${version%/bin/node}"
        minor="${version#*.}"
        patch="${minor#*.}"
        minor="${minor%%.*}"
        case "$minor:$patch" in *[!0-9:]*|:) continue ;; esac
        if [ "$minor" -gt "$best_minor" ] ||
          { [ "$minor" -eq "$best_minor" ] && [ "$patch" -gt "$best_patch" ]; }; then
          best_path="$candidate"
          best_minor="$minor"
          best_patch="$patch"
        fi
      done
      [ -z "$best_path" ] || run_if_node "$best_path" "$@"
      ;;
  esac
fi

# System-wide locations, searched last. Overridable via OSTROM_NODE_FALLBACKS
# (space-separated paths) so a user can pin a specific runtime — and so the
# test suite can exercise the not-found path on a machine that happens to have
# a node in one of these directories. CI runners do; this developer machine
# does not, which is precisely the kind of environment assumption this shim
# exists to stop making.
: "${OSTROM_NODE_FALLBACKS=/usr/local/bin/node /opt/homebrew/bin/node $HOME/.local/bin/node}"

# shellcheck disable=SC2086  # deliberate word-splitting: the override is a path list
for candidate in \
  "${FNM_DIR:-$HOME/.local/share/fnm}/aliases/default/bin/node" \
  "$HOME/.fnm/aliases/default/bin/node" \
  "${VOLTA_HOME:-$HOME/.volta}/bin/node" \
  "${ASDF_DATA_DIR:-$HOME/.asdf}/shims/node" \
  $OSTROM_NODE_FALLBACKS
do
  run_if_node "$candidate" "$@"
done

printf '%s\n' \
  "ostrom doctor: Node.js 18+ was not found; install Node or set nvm's default alias." >&2
exit 1
