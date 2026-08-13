#!/usr/bin/env bash
# run-node.sh — find Node.js or run the bundled /ostrom:doctor tool with it.
#
# WHY THIS SHIM EXISTS:
# nvm is normally loaded by an interactive shell profile. Claude Code hooks
# and plugin scripts run non-interactively, so a perfectly healthy nvm-managed
# `node` often is not on PATH. /ostrom:doctor is on-demand tooling and may rely on
# Node, but it must resolve the user's installed runtime without sourcing an
# interactive profile or changing the machine.
#
# Resolution is first-hit-wins: PATH, nvm's default alias, fnm, volta, asdf,
# then conventional standalone install locations. --resolve-only exposes the
# same resolver to other plugin scripts without duplicating that search order.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
resolve_only=0
if [ "${1:-}" = --resolve-only ]; then
  resolve_only=1
  shift
fi

absolute_executable() {
  candidate="$1"
  [ -x "$candidate" ] || return 1
  case "$candidate" in
    /*) printf '%s\n' "$candidate" ;;
    *)
      candidate_dir="$(dirname "$candidate")"
      candidate_name="$(basename "$candidate")"
      candidate_dir="$(cd -P "$candidate_dir" 2>/dev/null && pwd)" || return 1
      printf '%s/%s\n' "$candidate_dir" "$candidate_name"
      ;;
  esac
}

resolve_node() {
  path_node="$(command -v node 2>/dev/null)" || path_node=""
  if [ -n "$path_node" ]; then
    absolute_executable "$path_node" && return 0
  fi

  nvm_dir="${NVM_DIR:-$HOME/.nvm}"
  nvm_alias="$nvm_dir/alias/default"
  if [ -f "$nvm_alias" ]; then
    default="$(sed -n '1{s/[[:space:]]//g;p;}' "$nvm_alias")"
    case "$default" in
      v[0-9]*.[0-9]*.[0-9]*|[0-9]*.[0-9]*.[0-9]*)
        version="${default#v}"
        absolute_executable "$nvm_dir/versions/node/v$version/bin/node" && return 0
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
        if [ -n "$best_path" ]; then
          absolute_executable "$best_path" && return 0
        fi
        ;;
    esac
  fi

  # System-wide locations, searched last. Overridable via
  # OSTROM_NODE_FALLBACKS (space-separated paths) so a user can pin a specific
  # runtime — and so the test suite can exercise the not-found path on a
  # machine that happens to have a node in one of these directories. CI
  # runners do; this developer machine does not, which is precisely the kind
  # of environment assumption this shim exists to stop making.
  : "${OSTROM_NODE_FALLBACKS=/usr/local/bin/node /opt/homebrew/bin/node $HOME/.local/bin/node}"

  # shellcheck disable=SC2086  # deliberate word-splitting: the override is a path list
  for candidate in \
    "${FNM_DIR:-$HOME/.local/share/fnm}/aliases/default/bin/node" \
    "$HOME/.fnm/aliases/default/bin/node" \
    "${VOLTA_HOME:-$HOME/.volta}/bin/node" \
    "${ASDF_DATA_DIR:-$HOME/.asdf}/shims/node" \
    $OSTROM_NODE_FALLBACKS
  do
    absolute_executable "$candidate" && return 0
  done

  return 1
}

node_path="$(resolve_node)" || {
  if [ "$resolve_only" -eq 1 ]; then
    printf '%s\n' \
      "ostrom node resolver: Node.js 18+ was not found; install Node or set nvm's default alias." >&2
  else
    printf '%s\n' \
      "ostrom doctor: Node.js 18+ was not found; install Node or set nvm's default alias." >&2
  fi
  exit 1
}

if [ "$resolve_only" -eq 1 ]; then
  printf '%s\n' "$node_path"
  exit 0
fi

exec "$node_path" "$PLUGIN_ROOT/dist/doctor.js" "$@"
