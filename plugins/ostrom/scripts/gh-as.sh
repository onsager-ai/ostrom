#!/usr/bin/env bash
# Run one gh command under a freshly minted GitHub App installation token.
#
# A non-interactive session cannot mint a token for itself: capturing one
# requires `token="$(app-token.sh ...)"`, and the Claude Code Bash tool refuses
# a command containing a substitution it cannot statically analyze. With no
# prompt to fall back to, the mint is denied and the loop stops (#93). Moving
# the substitution inside this process boundary leaves the caller one
# statically-analyzable command:
#
#   bash "${CLAUDE_PLUGIN_ROOT}/scripts/gh-as.sh" <role> <owner>/<repo> <gh args...>
#
# The token exists only in this process and in the environment of the gh it
# execs. It is never printed, never written to disk, and never reaches the
# calling session, so there is nothing for the caller to unset afterwards.
#
# Every failure of the wrapper itself exits 2 after printing a `gh-as: ` line
# on stderr; that prefix is what distinguishes a wrapper failure from gh's own
# exit status, which passes through untouched on the success path.

# A caller may have tracing enabled. Disable it before credentials enter shell
# variables or command input, and never enable it again in this process.
set +x
set -uo pipefail
umask 077

fail() {
  printf 'gh-as: %s\n' "$1" >&2
  exit 2
}

usage='usage: gh-as.sh <role> <owner>/<repo> <gh args...>'

if [ "$#" -lt 3 ]; then
  fail "$usage"
fi

# Role and repository are validated exactly as app-token.sh validates them, so
# a malformed argument is refused here rather than travelling one process
# further before being refused with a different message.
role="$1"
case "$role" in
  ''|[!a-z]*|*[!a-z0-9_-]*)
    fail "invalid role: must match [a-z][a-z0-9_-]*"
    ;;
esac

repository="$2"
case "$repository" in
  */*) ;;
  *) fail "$usage" ;;
esac
owner="${repository%%/*}"
repo="${repository#*/}"
case "$owner" in
  ''|*[!A-Za-z0-9_.-]*) fail "$usage" ;;
esac
case "$repo" in
  ''|*/*|*[!A-Za-z0-9_.-]*) fail "$usage" ;;
esac
unset owner repo
shift 2

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v gh >/dev/null 2>&1 || fail "required command is unavailable: gh"

# Discard inherited credentials before the mint, not after it. If the mint
# fails there is then nothing left in this environment for gh to fall back to
# — falling through to the principal's identity is the failure this wrapper
# exists to make impossible, so it must not depend on the exit path taken.
unset GH_TOKEN GITHUB_TOKEN

gh_as_token="$(bash "$SCRIPT_DIR/app-token.sh" "$role" "$repository")"
gh_as_status=$?
if [ "$gh_as_status" -ne 0 ] || [ -z "$gh_as_token" ]; then
  unset gh_as_token
  fail "GitHub App authentication failed for $role on $repository; refusing to run gh with ambient credentials"
fi

export GH_TOKEN="$gh_as_token"
unset gh_as_token gh_as_status
exec gh "$@"
