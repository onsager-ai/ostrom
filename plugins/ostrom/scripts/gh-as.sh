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
# An older git silently ignores the GIT_CONFIG_COUNT/KEY/VALUE environment
# form instead of erroring on it, so this script cannot tell "the override
# took" from "the override was dropped and git fell back to whatever
# credential.helper the operator already has configured" by watching the
# exec'd process's exit code alone. When the wrapped command is `git`
# itself, this script instead checks its version up front and refuses to
# run at all below 2.31, before a token is even minted — there is no
# reason to mint a live token for a command that will not be allowed to
# run. The check runs "$1" --version, not a bare `git --version`: the
# wrapped command may be given as a path (`/opt/git-2.20/bin/git`) rather
# than a bare name resolved off PATH, and it is that exact binary — not
# whatever `git` happens to mean in this script's own PATH — that will
# receive the exec and do the actual push. That check keys off the wrapped
# command being `git` itself, not off whether it happens to invoke `git`:
# a wrapped *script* that calls `git` internally (such as `gate.sh`) is
# not covered, because this script has no way to know what binaries that
# script will run.
#
# Known commands keep the compatibility form below and have their scope
# derived from the action. Internal callers may instead put explicit
# --repositories/--permissions options and `--` after the leading role and
# repository.
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

usage='usage: gh-as.sh <role> <owner>/<repo> [--repositories <repo[,repo...]> --permissions <permission:level[,permission:level...]> --] <command> [args...]'

if [ "$#" -lt 3 ]; then
  fail "$usage"
fi
role="$1"
repository="$2"
shift 2

repositories=""
permissions=""
case "${1:-}" in
  --repositories|--permissions)
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
      case "$1" in
        --repositories)
          [ -z "$repositories" ] || fail "caller scope is invalid: --repositories was supplied more than once"
          [ "$#" -ge 2 ] || fail "$usage"
          repositories="$2"
          shift 2
          ;;
        --permissions)
          [ -z "$permissions" ] || fail "caller scope is invalid: --permissions was supplied more than once"
          [ "$#" -ge 2 ] || fail "$usage"
          permissions="$2"
          shift 2
          ;;
        *) fail "$usage" ;;
      esac
    done
    [ -n "$repositories" ] || \
      fail "unscoped token request rejected: caller must supply --repositories"
    [ -n "$permissions" ] || \
      fail "unscoped token request rejected: caller must supply --permissions"
    [ "$#" -gt 0 ] && [ "$1" = "--" ] || fail "$usage"
    shift
    [ "$#" -gt 0 ] || fail "$usage"
    ;;
esac

derive_permissions() {
  command_name="${1##*/}"
  case "$command_name" in
    git)
      shift
      for git_argument in "$@"; do
        case "$git_argument" in
          fetch|clone|ls-remote)
            printf '%s\n' 'metadata:read,contents:read'
            return 0
            ;;
          push)
            printf '%s\n' 'metadata:read,contents:write'
            return 0
            ;;
        esac
      done
      ;;
    gh)
      case "${2:-}:${3:-}" in
        repo:view) printf '%s\n' 'metadata:read'; return 0 ;;
        repo:clone) printf '%s\n' 'metadata:read,contents:read'; return 0 ;;
        issue:view|issue:list|issue:status)
          printf '%s\n' 'metadata:read,issues:read'; return 0
          ;;
        issue:comment|issue:create|issue:edit|issue:close|issue:reopen)
          printf '%s\n' 'metadata:read,issues:write'; return 0
          ;;
        pr:list|pr:diff|pr:checks|pr:status)
          printf '%s\n' 'metadata:read,pull_requests:read'; return 0
          ;;
        pr:view)
          if [[ " $* " == *" closingIssuesReferences "* ]]; then
            printf '%s\n' 'metadata:read,issues:read,pull_requests:read'
          else
            printf '%s\n' 'metadata:read,pull_requests:read'
          fi
          return 0
          ;;
        pr:comment)
          printf '%s\n' 'metadata:read,issues:write,pull_requests:read'; return 0
          ;;
        pr:create)
          printf '%s\n' 'metadata:read,pull_requests:write'; return 0
          ;;
        pr:merge)
          printf '%s\n' 'metadata:read,contents:write,pull_requests:write'; return 0
          ;;
        run:list) printf '%s\n' 'metadata:read,actions:read'; return 0 ;;
        api:graphql)
          # The only direct protocol call is review-thread resolution. Read
          # GraphQL calls live inside gate.sh and use that script's read scope.
          printf '%s\n' 'metadata:read,pull_requests:write'; return 0
          ;;
      esac
      ;;
    bash)
      script_name="${2:-}"
      case "${script_name##*/}" in
        gate.sh)
          printf '%s\n' 'metadata:read,issues:read,pull_requests:read,checks:read,statuses:read'
          return 0
          ;;
      esac
      ;;
  esac
  return 1
}

if [ -z "$repositories" ]; then
  repositories="$repository"
  permissions="$(derive_permissions "$@")" || \
    fail "unscoped token request rejected: command has no known scope; supply --repositories and --permissions"
fi

# See the comment block above: the GIT_CONFIG_COUNT/KEY/VALUE override this
# script relies on is only honoured from Git 2.31 onward, and an older git
# ignores it rather than erroring, so this has to be checked before doing
# anything else — including minting a token nothing will end up using.
case "$1" in
  git | */git)
    # Probe "$1" itself, not a bare `git` -- the wrapped command may be a
    # path to a specific git binary, and that is the one that will
    # actually exec and push, regardless of what `git` resolves to here.
    git_version_output="$("$1" --version 2>/dev/null)" ||
      fail "cannot enforce the ephemeral git credential helper: $1 --version failed, so a git push would silently fall back to the operator's own credentials"
    case "$git_version_output" in
      git\ version\ [0-9]*)
        git_version="${git_version_output#git version }"
        git_version="${git_version%% *}"
        ;;
      *)
        fail "cannot enforce the ephemeral git credential helper: could not parse a version from \"$git_version_output\", so a git push would silently fall back to the operator's own credentials"
        ;;
    esac
    git_minor="${git_version#*.}"
    git_major="${git_version%%.*}"
    git_minor="${git_minor%%.*}"
    case "$git_major" in
      '' | *[!0-9]*)
        fail "cannot enforce the ephemeral git credential helper: could not parse a version from \"$git_version_output\", so a git push would silently fall back to the operator's own credentials"
        ;;
    esac
    case "$git_minor" in
      '' | *[!0-9]*)
        fail "cannot enforce the ephemeral git credential helper: could not parse a version from \"$git_version_output\", so a git push would silently fall back to the operator's own credentials"
        ;;
    esac
    if [ "$git_major" -lt 2 ] || { [ "$git_major" -eq 2 ] && [ "$git_minor" -lt 31 ]; }; then
      fail "cannot enforce the ephemeral git credential helper: git $git_version is older than 2.31, so a git push would silently fall back to the operator's own credentials"
    fi
    unset git_version_output git_version git_major git_minor
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

token="$(bash "$SCRIPT_DIR/app-token.sh" "$role" "$repository" \
  --repositories "$repositories" --permissions "$permissions")" ||
  fail "could not mint a $role token for $repository"
if [ -z "$token" ]; then
  fail "minted an empty $role token for $repository"
fi

export GH_TOKEN="$token" GITHUB_TOKEN="$token"
export GIT_CONFIG_COUNT=2
export GIT_CONFIG_KEY_0="credential.helper" GIT_CONFIG_VALUE_0=""
export GIT_CONFIG_KEY_1="credential.helper" GIT_CONFIG_VALUE_1="!gh auth git-credential"
unset token role repository repositories permissions SCRIPT_DIR
exec "$@"
