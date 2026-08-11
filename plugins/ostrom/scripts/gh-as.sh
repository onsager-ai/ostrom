#!/usr/bin/env bash
# Run one command authenticated as a delivery role's GitHub App installation,
# without ever letting the minted token cross back into the caller's own
# shell state.
#
# A session's Bash tool statically rejects any command it is asked to run
# that contains command substitution, so a session cannot itself capture
# `app-token.sh`'s stdout into a variable (`token="$(app-token.sh ...)"`) —
# that assignment is exactly the shape that gets refused, and refusal happens
# before permission matching, so no allow rule can fix it. Writing the token
# to a file for the caller to read back has the same problem (`$(cat file)`
# is still a substitution) and is worse: it puts a live token on disk.
#
# This script is the one place the substitution happens. A session invokes
# it as a single flat command — role, repository, then the command to run —
# which the Bash tool parses as an ordinary argument list, not as shell it
# has to evaluate. The token is minted inside this process, exported only
# into this process's environment, and handed to the given command by
# `exec`, which replaces this process rather than spawning a child that
# could outlive the token or leak it back to a parent's variable. Nothing
# this script writes to stdout or stderr on its own path ever includes the
# token; only the exec'd command's own output follows.
#
# Usage: gh-as.sh <role> <owner>/<repo> <command> [args...]
#
# Exit code 111 means this script itself did not authenticate — a usage
# error, a mint failure, or an empty token — and <command> never ran. Any
# other exit code is <command>'s own, unchanged. 111 is reserved so a caller
# can always tell "authentication failed" apart from "the wrapped command
# ran and returned this code," including when the wrapped command is another
# script (such as gate.sh) with its own small exit-code space.

set +x
set -euo pipefail

EXIT_AUTH_FAILURE=111

fail() {
  printf 'gh-as: %s\n' "$1" >&2
  exit "$EXIT_AUTH_FAILURE"
}

if [ "$#" -lt 3 ]; then
  fail "usage: gh-as.sh <role> <owner>/<repo> <command> [args...]"
fi
role="$1"
repository="$2"
shift 2

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

token="$(bash "$SCRIPT_DIR/app-token.sh" "$role" "$repository")" ||
  fail "could not mint a $role token for $repository"
if [ -z "$token" ]; then
  fail "minted an empty $role token for $repository"
fi

export GH_TOKEN="$token" GITHUB_TOKEN="$token"
unset token role repository SCRIPT_DIR
exec "$@"
