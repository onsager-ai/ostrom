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
# `git` does not read GH_TOKEN itself — only `gh` does — so a `git push`
# needs a credential helper to bridge the two. This script points git at
# `gh auth git-credential`, which does read GH_TOKEN, for exactly the
# duration of the exec'd process, using the GIT_CONFIG_COUNT/KEY/VALUE
# environment form (supported since Git 2.31) rather than `git config` or
# `gh auth setup-git`: both of those write to a config file — `~/.gitconfig`
# or `.git/config` — and would leave a helper reference behind after this
# process exits. An environment variable dies with the process that set it.
# The first pair clears any credential.helper already configured in
# `~/.gitconfig` or the repository's own config (an operator's personal
# helper would otherwise be tried first and could satisfy the request with
# the operator's own stored credentials, silently defeating the whole
# point); the second pair is the only helper left standing. Harmless for a
# wrapped command that never calls `git`.
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
export GIT_CONFIG_COUNT=2
export GIT_CONFIG_KEY_0="credential.helper" GIT_CONFIG_VALUE_0=""
export GIT_CONFIG_KEY_1="credential.helper" GIT_CONFIG_VALUE_1="!gh auth git-credential"
unset token role repository SCRIPT_DIR
exec "$@"
