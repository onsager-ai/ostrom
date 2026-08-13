#!/usr/bin/env bash

# -E (errtrace) so the ERR trap below is inherited by functions, subshells
# and command substitutions. Most assertions here run inside `( cd ...; ... )`
# subshells or `$( ... )` captures; without it the trap is silently absent
# exactly where the suite does most of its work, and a failure there prints
# nothing at all.
set -Eeuo pipefail

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
# set -e aborts on the first failing assertion with no indication of which
# one — a bare `exit 1` gives no line number, no expected/got. Report where
# it died before the shell unwinds. Bash fires the ERR trap for any
# non-zero command regardless of the current errexit state, so guard on
# $- to skip the deliberate, already-handled failures inside set +e blocks
# (e.g. capturing a killed process's wait status).
trap '[[ $- == *e* ]] && echo "mandate tests: FAILED at test.sh:${LINENO} (last command: ${BASH_COMMAND})" >&2; true' ERR

# Shipped plugin files must not retain private checkout paths. Build the
# expression in pieces so this assertion does not match its own source.
machine_path_pattern='~[/]projects[/]|[/]home[/]|dot''claude'
if grep -R -I -n -E "$machine_path_pattern" \
  --exclude-dir=node_modules "$PLUGIN_ROOT"; then
  echo "mandate tests: shipped plugin contains a machine-specific path" >&2
  exit 1
fi

mkdir -p "$fixture/config/ostrom" "$fixture/repo" "$fixture/bin"
export MANDATE_SWEEP_TIME="2026-08-01T00:00:00Z"
export MANDATE_TODAY="2026-08-01"
export MANDATE_NOW_EPOCH="1785542400"

write_config() {
  delegated_selector="${1:-label:maintenance}"
  cat >"$fixture/config/ostrom/mandates.yaml" <<YAML
provider: file
cadence_hours: 24
stuck_after_days: 1
bounce_all:
  - title:*credential*
  - title:*never fires*
projects:
  - repo: example-org/example-repo
    delegated:
      - $delegated_selector
      - scope:tooling
      - path:docs/**
    excluded:
      - label:ignored
    reserved:
      - 10
    default: unclassified
    paused: false
    bounce:
      - path:rules/frozen-rules.md
  - repo: example-org/another-repo
    delegated:
      - label:maintenance
    excluded: []
    reserved: []
    default: excluded
    paused: true
    bounce:
      - title:*production deployment*
YAML
}
write_config

# The gatekeeper instructions are the callers: they acquire/release the lease
# and name every required trace event without consulting the trace for a lock.
gatekeep_skill="$PLUGIN_ROOT/skills/gatekeep/SKILL.md"
merge_skill="$PLUGIN_ROOT/skills/merge/SKILL.md"
work_skill="$PLUGIN_ROOT/skills/work/SKILL.md"
role_boundary_doc="$PLUGIN_ROOT/../../docs/role-permission-boundaries.md"
work_frontmatter="$(
  awk 'NR == 1 { next } /^---$/ { exit } { print }' "$work_skill"
)"
grep -q 'lease.sh\" acquire "\$lease_owner"' "$gatekeep_skill"
grep -q 'lease.sh\" release "\$lease_owner"' "$gatekeep_skill"
grep -q 'Never infer concurrency or lease' "$gatekeep_skill"
for trace_kind in pass-started item-selected pass-ended; do
  grep -q "trace.sh\" append $trace_kind" "$gatekeep_skill"
done

# #151: one repository's token-mint failure is a visible, bounded skip rather
# than the end of the whole gatekeeper pass. These are protocol-contract tests
# because gatekeeping is implemented by gatekeep/SKILL.md rather than by a
# driver script. The continuation assertion is the regression case: the old
# protocol said to stop the iteration and therefore fails this check.
if grep -Fq 'stop this iteration' "$gatekeep_skill"; then
  echo "gatekeeper protocol must distinguish a repository skip from ending the pass" >&2
  exit 1
fi
grep -Fq 'exact same `gh-as.sh` invocation once immediately' \
  "$gatekeep_skill"
grep -Fq 'continue to the next repository' "$gatekeep_skill"
grep -Fq 'Continue enumerating every other roster repository' "$gatekeep_skill"
grep -Fq 'Judge every candidate gathered' "$gatekeep_skill"
grep -Fq 'even when `skipped_repos` is not' "$gatekeep_skill"

# A skip is reported both to the principal and in the terminal fact. The
# completed count remains independent of the skipped-repository list, and a
# pass that reaches the end with skips has the explicit non-error outcome.
grep -Fq 'repository once to `skipped_repos`' "$gatekeep_skill"
grep -Fq -- '--argjson completed "$completed_candidates"' "$gatekeep_skill"
grep -Fq -- '--argjson skipped "$skipped_repos"' "$gatekeep_skill"
grep -Fq 'skipped_repos: $skipped' "$gatekeep_skill"
if grep -Fq 'failed_repo' "$gatekeep_skill"; then
  echo "gatekeeper terminal fact must carry skipped_repos, not one failed_repo" >&2
  exit 1
fi
grep -Fq 'existing outcome `completed`' "$gatekeep_skill"
grep -Fq 'Use outcome `partial`' "$gatekeep_skill"
grep -Fq 'a productive pass with skips is not' "$gatekeep_skill"

# Missing or malformed credentials are session-wide, not a repository skip:
# they still end the pass. Neither that path nor the retry/skip path may ever
# escape the App blast radius through an ambient principal credential.
grep -Fq '**Credentials cannot be loaded at all.**' "$gatekeep_skill"
grep -Fq 'outcome to `error`' "$gatekeep_skill"
grep -Fq 'release the lease, and end the' "$gatekeep_skill"
grep -Fq '**No exit-`111` path may run the command under an ambient credential' \
  "$gatekeep_skill"
for trace_kind in artifact-produced gate-verdict-consumed; do
  grep -q "trace.sh\" append $trace_kind" "$merge_skill"
done
grep -q 'MANDATE_LEASE_NAME=builder.lease' "$work_skill"
grep -q 'scripts/sweep.sh' "$work_skill"
grep -q '^argument-hint: "\[optional queue focus, e.g. project name or item class\]"$' \
  <<<"$work_frontmatter"
grep -q 'invocation input as a natural-language filter' "$work_skill"
! grep -q '\$ARGUMENTS' "$work_skill"
grep -q 'builder-<session>-wake<N>' "$work_skill"
for trace_kind in pass-started item-worked pass-ended; do
  grep -q "trace.sh\" append $trace_kind" "$work_skill"
done

# One App erases actor-level role attribution, so every authoring protocol
# carries a self-asserted role marker while every consumer says that marker is
# advisory rather than turning it back into an identity control.
grep -q 'Ostrom-Role: builder' "$work_skill"
grep -q 'Ostrom-Role: builder' "$gatekeep_skill"
# The gatekeeper must NOT stamp the merge commit. `gh pr merge --body`
# replaces the squash commit message rather than appending to it, and that
# default message is the only thing carrying the builder's own trailer onto
# the default branch. Stamping the merge would erase more attribution than it
# adds; the gatekeeper's role is in its `decision-taken` record instead.
# Written as an `if`, not `! grep`: bash exempts `!`-inverted commands from
# `set -e` and from the ERR trap, so a leading `!` here would assert nothing.
if grep -qE 'gh pr merge[^`]*--body' "$merge_skill"; then
  echo "merge protocol must not pass --body to gh pr merge" >&2
  exit 1
fi

# The gatekeeper must not approve. Since #107 every delivery role
# authenticates as the same App, so the App that authored the pull request is
# the App that would review it, and GitHub refuses self-approval outright.
# The first armed gatekeeper pass halted on exactly this. The verdict is
# recorded as a PR comment and a decision-taken record instead. Same `if`
# form: `!` would be exempt from set -e and assert nothing.
# Matched as an invocation -- routed through gh-as.sh, as every protocol call
# is -- so the prose below that names the forbidden command does not trip it.
if grep -qE 'gh-as\.sh[^`]*gh pr review[^`]*--approve' "$merge_skill" "$gatekeep_skill"; then
  echo "merge protocol must not approve: one App cannot approve its own PR" >&2
  exit 1
fi
grep -q 'Do not approve' "$merge_skill"
grep -q 'gh pr comment <PR number> --repo <owner/repo> --body-file <file>' \
  "$merge_skill"
grep -q 'Ostrom-Role: builder' "$merge_skill"
for role_doc in "$work_skill" "$gatekeep_skill" "$merge_skill"; do
  grep -qi 'advisory' "$role_doc"
  grep -qi 'evidence' "$role_doc"
done
grep -q 'The harness is the enforcement boundary. The App is the blast radius.' \
  "$role_boundary_doc"
grep -q 'no gate, audit, or authorization decision may treat it as proof' \
  "$role_boundary_doc"

# #80's reversal half: the gatekeeper records a decision-taken trace record
# with a reversal pointer at every point it exercises its own judgment —
# merging and resolving a review thread — and the builder does the same for
# filing and closing an issue. Each fact carries role, owner, repo, ref,
# decision, and reversal; reasoning is narration, per trace.sh's own split.
[ "$(grep -c "trace.sh\" append decision-taken" "$merge_skill")" -eq 2 ]
[ "$(grep -c "trace.sh\" append decision-taken" "$work_skill")" -eq 1 ]
for skill_file in "$merge_skill" "$work_skill"; do
  grep -q 'role: "gatekeeper"\|role: "builder"' "$skill_file"
  grep -q 'reversal: \$reversal' "$skill_file"
done
grep -q 'unresolve thread' "$merge_skill"
grep -q 'revert .*: open a revert pull request' "$merge_skill"
grep -q 'close <repo>#<new issue number>' "$work_skill"
grep -q 'reopen <repo>#<ref>' "$work_skill"

# The systemd wrapper fails closed when disarmed, backs off on its own outer
# lease, preserves its role identity across processes, records measured cost,
# and finalizes a signalled pass before releasing that lease.
pass_fixture="$fixture/pass"
pass_config="$pass_fixture/config"
fake_claude="$pass_fixture/fake-claude"
fake_marker="$pass_fixture/claude-started"
mkdir -p "$pass_config/ostrom/roles"
printf '{}\n' >"$pass_config/ostrom/roles/builder.settings.json"
printf '{}\n' >"$pass_config/ostrom/roles/gatekeeper.settings.json"
cat >"$fake_claude" <<'SH'
#!/usr/bin/env bash
if [ -n "${FAKE_CLAUDE_ARGS_FILE:-}" ]; then
  printf '%s\n' "$@" >"$FAKE_CLAUDE_ARGS_FILE"
fi
# Real work/SKILL.md and gatekeep/SKILL.md sessions append their own
# pass-started row, under their own minted owner, once they reach step 2 of
# the protocol. FAKE_CLAUDE_INNER_OWNER opts a fixture into simulating that;
# leaving it unset simulates a session that never got that far -- the
# no-op shape pass.sh must now catch.
if [ -n "${FAKE_CLAUDE_INNER_OWNER:-}" ]; then
  bash "$FAKE_CLAUDE_TRACE_SH" append pass-started \
    "$(printf '{"owner":"%s"}' "$FAKE_CLAUDE_INNER_OWNER")" '{}' >/dev/null
fi
# Gatekeeper pass-ended facts deliberately omit owner, matching the weaker
# production shape pass.sh must still reconcile with the role-prefixed start.
if [ -n "${FAKE_CLAUDE_INNER_OUTCOME:-}" ]; then
  bash "$FAKE_CLAUDE_TRACE_SH" append pass-ended \
    "$(printf '{"outcome":"%s","completed_candidates":0}' "$FAKE_CLAUDE_INNER_OUTCOME")" \
    '{}' >/dev/null
fi
case "${FAKE_CLAUDE_MODE:-complete}" in
  complete)
    printf '%s\n' '{"type":"assistant","message":"placeholder"}'
    printf '%s\n' '{"type":"result","total_cost_usd":1.25}'
    ;;
  wait)
    printf '%s\n' "$$" >"$FAKE_CLAUDE_MARKER"
    trap 'exit 143' TERM
    while :; do sleep 1; done
    ;;
  *)
    exit 42
    ;;
esac
SH
chmod +x "$fake_claude"
fake_claude_trace_sh="$PLUGIN_ROOT/scripts/trace.sh"

CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$fake_marker" ]
[ ! -e "$pass_config/ostrom/sprint.jsonl" ]

: >"$pass_config/ostrom/loop-armed"
# Stamp the fixture lease at real time, not a fixed epoch. A lease started at
# epoch 400 is decades expired by the time pass.sh reads it, so pass.sh
# correctly reclaims it and runs a full pass — which exercises reclamation,
# not the timer overlap this case exists to cover.
CLAUDE_CONFIG_DIR="$pass_config" \
  MANDATE_LEASE_NAME=builder-pass.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire fixture-holder 3600 >/dev/null
CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$fake_marker" ]
[ ! -e "$pass_config/ostrom/sprint.jsonl" ]
CLAUDE_CONFIG_DIR="$pass_config" MANDATE_LEASE_NAME=builder-pass.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release fixture-holder

builder_args="$pass_fixture/builder-args"
CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" FAKE_CLAUDE_ARGS_FILE="$builder_args" \
  FAKE_CLAUDE_INNER_OWNER="builder-inner-session-wake1" \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ ! -e "$pass_config/ostrom/builder-pass.lease" ]
# The regression test for #73: a pass whose inner session did take ownership
# -- proven by its own pass-started row landing after the wrapper's -- is
# still reported completed, never collapsed into no-op just because a
# second row now exists in the trace.
jq -s -e '
  length == 3
  and map(.kind) == ["pass-started", "pass-started", "pass-ended"]
  and (.[0].fact.owner | test("^builder-[0-9a-f]{8}-wake1$"))
  and .[1].fact.owner == "builder-inner-session-wake1"
  and .[2].fact.owner == .[0].fact.owner
  and .[2].fact.outcome == "completed"
  and (.[2].fact | has("reason") | not)
  and .[2].fact.cost_usd == 1.25
  and (.[2].fact.duration_seconds | type == "number" and . >= 0)
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

# #99 (production path): the rest of this suite exports MANDATE_NOW_EPOCH
# globally (above) so every simulated day is deterministic, so this fixture
# gets its own config dir and explicitly unsets it for one call -- the one
# way to exercise what a real deployment does, where no caller ever sets
# MANDATE_NOW_EPOCH at all. pass.sh must keep stamping the pass-started/
# pass-ended rows it writes about its own pass with the real wall clock
# exactly as before this fix, so trace.sh's own real-UTC default keeps doing
# the stamping and production behaviour is unchanged.
pass_realclock="$fixture/pass-realclock"
pass_realclock_config="$pass_realclock/config"
mkdir -p "$pass_realclock_config/ostrom/roles"
printf '{}\n' >"$pass_realclock_config/ostrom/roles/builder.settings.json"
: >"$pass_realclock_config/ostrom/loop-armed"
# Read the real date on both sides of the run and accept either, so a pass
# that straddles UTC midnight does not make this assertion flake -- the
# claim is "the real clock, not the simulated day", not "this exact date".
real_today_before="$(date -u +%Y-%m-%d)"
env -u MANDATE_NOW_EPOCH \
  CLAUDE_CONFIG_DIR="$pass_realclock_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_INNER_OWNER="builder-inner-realclock-wake1" \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
real_today_after="$(date -u +%Y-%m-%d)"
jq -s -e \
  --arg before "$real_today_before" \
  --arg after "$real_today_after" '
  def on_a_real_day: (startswith($before) or startswith($after));
  length == 3
  and (.[0].ts | on_a_real_day)
  and (.[2].ts | on_a_real_day)
' "$pass_realclock_config/ostrom/sprint.jsonl" >/dev/null

# The permission mode is role-scoped, not hardcoded, and neither role may
# ever be handed the invalid "default" value.
[ "$(grep -A1 '^--permission-mode$' "$builder_args" | tail -n1)" = auto ]
! grep -qx default "$builder_args"

# The turn ceiling is a runaway-loop backstop set well above the wall-clock
# timeout that actually bounds a pass, not the old, easily-exceeded 40.
[ "$(grep -A1 '^--max-turns$' "$builder_args" | tail -n1)" = 200 ]
! grep -qx 40 "$builder_args"

CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  FAKE_CLAUDE_INNER_OWNER="builder-inner-session-wake2" \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
jq -s -e '
  length == 6
  and .[3].kind == "pass-started"
  and .[4].kind == "pass-started"
  and .[5].kind == "pass-ended"
  and (.[3].fact.owner | test("^builder-[0-9a-f]{8}-wake2$"))
  and .[4].fact.owner == "builder-inner-session-wake2"
  and (.[0].fact.owner | split("-wake")[0])
    == (.[3].fact.owner | split("-wake")[0])
  and .[5].fact.owner == .[3].fact.owner
  and .[5].fact.outcome == "completed"
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

# A pass whose child exits cleanly but whose inner session never appended its
# own pass-started row -- the exact shape measured in production, 19 times in
# a row -- is a no-op, not a completed pass: the wrapper ran, the protocol
# never took ownership, and that distinction must survive into the trace.
noop_args="$pass_fixture/noop-args"
CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" FAKE_CLAUDE_ARGS_FILE="$noop_args" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
jq -s -e '
  length == 8
  and .[6].kind == "pass-started"
  and .[7].kind == "pass-ended"
  and (.[6].fact.owner | test("^builder-[0-9a-f]{8}-wake3$"))
  and .[7].fact.owner == .[6].fact.owner
  and .[7].fact.outcome == "no-op"
  and .[7].fact.reason == "blocked"
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

# A child that exits non-zero before the inner session ever took ownership is
# still a failure, not a no-op -- a crash before the protocol starts is not a
# legitimate skip, and must not borrow no-op's quiet reporting.
CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" FAKE_CLAUDE_MODE=fail \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1 || true
jq -s -e '
  length == 10
  and .[8].kind == "pass-started"
  and .[9].kind == "pass-ended"
  and (.[8].fact.owner | test("^builder-[0-9a-f]{8}-wake4$"))
  and .[9].fact.owner == .[8].fact.owner
  and .[9].fact.outcome == "failed"
  and (.[9].fact | has("reason") | not)
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

# Once the inner protocol records its own result, a clean client exit cannot
# overwrite that better-informed outcome. The gatekeeper-shaped inner end row
# has no owner, so correlation comes from its role-prefixed start and this
# wrapper's post-start watermark rather than a field production does not emit.
outcome_fixture="$fixture/inner-outcome"
outcome_config="$outcome_fixture/config"
outcome_marker="$outcome_fixture/claude-started"
mkdir -p "$outcome_config/ostrom/roles"
printf '{}\n' >"$outcome_config/ostrom/roles/gatekeeper.settings.json"
: >"$outcome_config/ostrom/loop-armed"

CLAUDE_CONFIG_DIR="$outcome_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_INNER_OWNER="gatekeeper-inner-fixture-wake1" \
  FAKE_CLAUDE_INNER_OUTCOME=failed \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null
jq -s -e '
  length == 4
  and map(.kind) == ["pass-started", "pass-started", "pass-ended", "pass-ended"]
  and .[1].fact.owner == "gatekeeper-inner-fixture-wake1"
  and (.[2].fact | has("owner") | not)
  and .[2].fact.outcome == "failed"
  and .[3].fact.owner == .[0].fact.owner
  and .[3].fact.outcome == "failed"
  and .[3].fact.cost_usd == 1.25
' "$outcome_config/ostrom/sprint.jsonl" >/dev/null

CLAUDE_CONFIG_DIR="$outcome_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_INNER_OWNER="gatekeeper-inner-fixture-wake2" \
  FAKE_CLAUDE_INNER_OUTCOME=completed \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null
jq -s -e '
  length == 8
  and .[6].kind == "pass-ended"
  and .[6].fact.outcome == "completed"
  and .[7].kind == "pass-ended"
  and .[7].fact.owner == .[4].fact.owner
  and .[7].fact.outcome == "completed"
' "$outcome_config/ostrom/sprint.jsonl" >/dev/null

# With no inner rows at all, #73's no-op classification and reason survive
# unchanged rather than being mistaken for a missing outcome on a real run.
CLAUDE_CONFIG_DIR="$outcome_config" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null
jq -s -e '
  length == 10
  and .[8].kind == "pass-started"
  and .[9].kind == "pass-ended"
  and .[9].fact.owner == .[8].fact.owner
  and .[9].fact.outcome == "no-op"
  and .[9].fact.reason == "blocked"
' "$outcome_config/ostrom/sprint.jsonl" >/dev/null

# Transport authority remains outside the protocol: systemd's TERM timeout
# and another trapped signal must override even an already-written inner row.
: >"$outcome_marker"
FAKE_CLAUDE_MODE=wait FAKE_CLAUDE_MARKER="$outcome_marker" \
  FAKE_CLAUDE_INNER_OWNER="gatekeeper-inner-fixture-wake4" \
  FAKE_CLAUDE_INNER_OUTCOME=failed \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  CLAUDE_CONFIG_DIR="$outcome_config" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null 2>&1 &
outcome_timeout_pid=$!
for _attempt in $(seq 1 100); do
  [ -s "$outcome_marker" ] && break
  sleep 0.05
done
[ -s "$outcome_marker" ]
kill -TERM "$outcome_timeout_pid"
set +e
wait "$outcome_timeout_pid"
outcome_timeout_status=$?
set -e
[ "$outcome_timeout_status" -eq 143 ]
jq -s -e '
  length == 14
  and .[12].fact.outcome == "failed"
  and .[13].fact.owner == .[10].fact.owner
  and .[13].fact.outcome == "timed-out"
' "$outcome_config/ostrom/sprint.jsonl" >/dev/null

: >"$outcome_marker"
FAKE_CLAUDE_MODE=wait FAKE_CLAUDE_MARKER="$outcome_marker" \
  FAKE_CLAUDE_INNER_OWNER="gatekeeper-inner-fixture-wake5" \
  FAKE_CLAUDE_INNER_OUTCOME=completed \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  CLAUDE_CONFIG_DIR="$outcome_config" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null 2>&1 &
outcome_signal_pid=$!
for _attempt in $(seq 1 100); do
  [ -s "$outcome_marker" ] && break
  sleep 0.05
done
[ -s "$outcome_marker" ]
kill -HUP "$outcome_signal_pid"
set +e
wait "$outcome_signal_pid"
outcome_signal_status=$?
set -e
[ "$outcome_signal_status" -eq 129 ]
jq -s -e '
  length == 18
  and .[16].fact.outcome == "completed"
  and .[17].fact.owner == .[14].fact.owner
  and .[17].fact.outcome == "failed"
' "$outcome_config/ostrom/sprint.jsonl" >/dev/null

gatekeeper_args="$pass_fixture/gatekeeper-args"
FAKE_CLAUDE_MODE=wait FAKE_CLAUDE_MARKER="$fake_marker" \
  FAKE_CLAUDE_ARGS_FILE="$gatekeeper_args" \
  CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" gatekeeper >/dev/null 2>&1 &
signalled_pass_pid=$!
for _attempt in $(seq 1 100); do
  [ -s "$fake_marker" ] && break
  sleep 0.05
done
[ -s "$fake_marker" ]
signalled_child_pid="$(cat "$fake_marker")"
kill -TERM "$signalled_pass_pid"
set +e
wait "$signalled_pass_pid"
signalled_pass_status=$?
set -e
[ "$signalled_pass_status" -eq 143 ]
[ ! -e "$pass_config/ostrom/gatekeeper-pass.lease" ]
! kill -0 "$signalled_child_pid" 2>/dev/null
jq -s -e '
  (map(select(.fact.owner? | startswith("gatekeeper-")))) as $gatekeeper
  | ($gatekeeper | length) == 2
  and ($gatekeeper | map(.kind)) == ["pass-started", "pass-ended"]
  and $gatekeeper[1].fact.owner == $gatekeeper[0].fact.owner
  and $gatekeeper[1].fact.outcome == "timed-out"
  and $gatekeeper[1].fact.cost_usd == null
  and ($gatekeeper[1].fact.duration_seconds | type == "number" and . >= 0)
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

# The gatekeeper's mode is `manual`, not the builder's `auto`, and never the
# invalid "default" value either.
[ "$(grep -A1 '^--permission-mode$' "$gatekeeper_args" | tail -n1)" = manual ]
! grep -qx default "$gatekeeper_args"

# Two concurrent gatekeeper-pass starts cannot both proceed. The winner writes
# the first trace record while the loser backs off without reading stale state.
lease_concurrent="$fixture/lease-concurrent"
mkdir -p "$lease_concurrent/ostrom"
concurrent_trace="$lease_concurrent/ostrom/sprint.jsonl"
[ ! -e "$concurrent_trace" ]

gatekeeper_pass_start() {
  pass_owner="$1"
  set +e
  CLAUDE_CONFIG_DIR="$lease_concurrent" MANDATE_LEASE_NOW_EPOCH=100 \
    bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$pass_owner" 60
  acquire_status=$?
  set -e
  if [ "$acquire_status" -ne 0 ]; then
    return "$acquire_status"
  fi
  MANDATE_TRACE_TIME="2026-08-01T00:00:00Z" \
    CLAUDE_CONFIG_DIR="$lease_concurrent" \
    bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-started \
      "$(jq -cn --arg owner "$pass_owner" '{owner: $owner}')" \
      '{}' >/dev/null
  printf '%s\n' "$pass_owner" >"$lease_concurrent/$pass_owner.proceeded"
}

set +e
gatekeeper_pass_start gatekeeper-alpha \
  >"$lease_concurrent/alpha.out" 2>"$lease_concurrent/alpha.err" &
lease_alpha_pid=$!
gatekeeper_pass_start gatekeeper-beta \
  >"$lease_concurrent/beta.out" 2>"$lease_concurrent/beta.err" &
lease_beta_pid=$!
wait "$lease_alpha_pid"
lease_alpha_status=$?
wait "$lease_beta_pid"
lease_beta_status=$?
set -e
[ $(( (lease_alpha_status == 0) + (lease_beta_status == 0) )) -eq 1 ]
[ $(( (lease_alpha_status == 3) + (lease_beta_status == 3) )) -eq 1 ]
concurrent_lease="$(
  CLAUDE_CONFIG_DIR="$lease_concurrent" \
    bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"
jq -e '
  (.owner == "gatekeeper-alpha" or .owner == "gatekeeper-beta")
  and .started_at == 100
  and .expires_at == 160
' <<<"$concurrent_lease" >/dev/null
[ -e "$lease_concurrent/gatekeeper-alpha.proceeded" ] || \
  [ -e "$lease_concurrent/gatekeeper-beta.proceeded" ]
[ ! -e "$lease_concurrent/gatekeeper-alpha.proceeded" ] || \
  [ ! -e "$lease_concurrent/gatekeeper-beta.proceeded" ]
[ -f "$concurrent_trace" ]
[ "$(wc -l <"$concurrent_trace" | tr -d '[:space:]')" -eq 1 ]
jq -e '.kind == "pass-started" and (.fact.owner | startswith("gatekeeper-"))' \
  "$concurrent_trace" >/dev/null

winning_owner="$(jq -r '.owner' <<<"$concurrent_lease")"
MANDATE_TRACE_TIME="2026-08-01T00:01:00Z" \
  CLAUDE_CONFIG_DIR="$lease_concurrent" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append item-selected \
    '{"repo":"example-org/example-repo","pr":51}' '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:02:00Z" \
  CLAUDE_CONFIG_DIR="$lease_concurrent" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append artifact-produced \
    '{"repo":"example-org/example-repo","pr":51,"head_sha":"0123456789abcdef"}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:03:00Z" \
  CLAUDE_CONFIG_DIR="$lease_concurrent" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append gate-verdict-consumed \
    '{"repo":"example-org/example-repo","pr":51,"head_sha":"0123456789abcdef","verdict":"pass","exit_code":0,"already_judged":false}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:04:00Z" \
  CLAUDE_CONFIG_DIR="$lease_concurrent" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"outcome":"complete","completed_candidates":1}' '{}' >/dev/null
jq -s -e 'map(.kind) == [
  "pass-started",
  "item-selected",
  "artifact-produced",
  "gate-verdict-consumed",
  "pass-ended"
]' "$concurrent_trace" >/dev/null
CLAUDE_CONFIG_DIR="$lease_concurrent" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release "$winning_owner"
[ ! -e "$lease_concurrent/ostrom/sprint.lease" ]

# Named leases isolate the two roles, including their mutation guards. A held
# gatekeeper lease and its guard do not block the builder lease; releasing the
# builder lease leaves the gatekeeper lease and guard untouched.
role_leases="$fixture/role-leases"
CLAUDE_CONFIG_DIR="$role_leases" MANDATE_LEASE_NOW_EPOCH=150 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire gatekeeper-alpha 60 >/dev/null
CLAUDE_CONFIG_DIR="$role_leases" MANDATE_LEASE_NOW_EPOCH=150 \
  MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-alpha 60 >/dev/null
jq -e '.owner == "gatekeeper-alpha"' \
  "$role_leases/ostrom/sprint.lease" >/dev/null
jq -e '.owner == "builder-alpha"' \
  "$role_leases/ostrom/builder.lease" >/dev/null
printf '%s\n' held >"$role_leases/ostrom/.sprint.lease.guard"
CLAUDE_CONFIG_DIR="$role_leases" MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release builder-alpha
[ ! -e "$role_leases/ostrom/builder.lease" ]
[ ! -e "$role_leases/ostrom/.builder.lease.guard" ]
[ -e "$role_leases/ostrom/sprint.lease" ]
[ -e "$role_leases/ostrom/.sprint.lease.guard" ]
rm -f "$role_leases/ostrom/.sprint.lease.guard"
CLAUDE_CONFIG_DIR="$role_leases" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release gatekeeper-alpha

# A second owner on the builder lease backs off before it can touch an item or
# append a trace row. The same named lease still mutually excludes its owners.
builder_overlap="$fixture/builder-overlap"
CLAUDE_CONFIG_DIR="$builder_overlap" MANDATE_LEASE_NOW_EPOCH=175 \
  MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-alpha-wake1 60 >/dev/null
set +e
CLAUDE_CONFIG_DIR="$builder_overlap" MANDATE_LEASE_NOW_EPOCH=175 \
  MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-beta-wake1 60 \
  >/dev/null 2>&1
builder_overlap_status=$?
set -e
[ "$builder_overlap_status" -eq 3 ]
[ ! -e "$builder_overlap/item-touched" ]
[ ! -e "$builder_overlap/ostrom/sprint.jsonl" ]
CLAUDE_CONFIG_DIR="$builder_overlap" MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release builder-alpha-wake1

# A mid-item builder failure remains durable and releases its named lease.
builder_failure="$fixture/builder-failure"
failure_owner="builder-fixture-wake1"
CLAUDE_CONFIG_DIR="$builder_failure" MANDATE_LEASE_NOW_EPOCH=190 \
  MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$failure_owner" 60 >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:00:00Z" \
  CLAUDE_CONFIG_DIR="$builder_failure" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-started \
    "$(jq -cn --arg owner "$failure_owner" '{owner: $owner}')" \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:01:00Z" \
  CLAUDE_CONFIG_DIR="$builder_failure" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append item-worked \
    "$(jq -cn --arg owner "$failure_owner" \
      '{owner: $owner, repo: "example-org/example-repo", ref: "#59",
        action: "test", outcome: "failed", exit_code: 42}')" \
    '{"reason":"fixture failure"}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T00:02:00Z" \
  CLAUDE_CONFIG_DIR="$builder_failure" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    "$(jq -cn --arg owner "$failure_owner" \
      '{owner: $owner, outcome: "failed", worked_items: 1}')" \
    '{"reason":"item failed"}' >/dev/null
CLAUDE_CONFIG_DIR="$builder_failure" MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release "$failure_owner"
[ ! -e "$builder_failure/ostrom/builder.lease" ]
jq -s -e '
  map(.kind) == ["pass-started", "item-worked", "pass-ended"]
  and all(.[]; .fact.owner == "builder-fixture-wake1")
  and .[1].fact.exit_code == 42
  and .[2].fact.outcome == "failed"
' "$builder_failure/ostrom/sprint.jsonl" >/dev/null

# The Claude session pass.sh spawns acquires its own protocol lease
# (builder.lease) as step 2 of its work. When pass.sh's child is killed
# before that session reaches its own release step, the inner lease must not
# outlive the pass -- that is the property pass.sh exists to guarantee, one
# layer deeper than the outer *-pass.lease it already protects.
inner_kill="$fixture/inner-lease-kill"
mkdir -p "$inner_kill/ostrom/roles"
printf '{}\n' >"$inner_kill/ostrom/roles/builder.settings.json"
: >"$inner_kill/ostrom/loop-armed"
inner_kill_marker="$inner_kill/claude-started"

FAKE_CLAUDE_MODE=wait FAKE_CLAUDE_MARKER="$inner_kill_marker" \
  CLAUDE_CONFIG_DIR="$inner_kill" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder \
  >"$inner_kill/pass.out" 2>"$inner_kill/pass.err" &
inner_kill_pass_pid=$!
for _attempt in $(seq 1 100); do
  [ -s "$inner_kill_marker" ] && break
  sleep 0.05
done
[ -s "$inner_kill_marker" ]

# Simulate the spawned session having reached step 2 of its own protocol and
# acquired the inner lease, using the real clock (no epoch override) so its
# started_at is provably at-or-after pass.sh's own recorded start_epoch.
CLAUDE_CONFIG_DIR="$inner_kill" MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-childsession-wake9 \
  >/dev/null

kill -TERM "$inner_kill_pass_pid"
set +e
wait "$inner_kill_pass_pid"
inner_kill_status=$?
set -e
[ "$inner_kill_status" -eq 143 ]
[ ! -e "$inner_kill/ostrom/builder-pass.lease" ]
[ ! -e "$inner_kill/ostrom/builder.lease" ]
grep -q 'releasing inner lease builder.lease held by builder-childsession-wake9' \
  "$inner_kill/pass.err"

# An inner lease already held by a concurrent interactive session -- one
# whose started_at predates this pass's own start -- must be left alone. This
# is the regression test for the safety check: pass.sh cannot tell that lease
# apart from its own child's by owner name, only by timestamp, and stealing
# it would break the one guarantee a concurrent session relies on.
#
# Stamp the fixture lease at a fixed, far-past epoch rather than the real
# clock. pass.sh reads its own start_epoch from the real clock, and lease.sh
# timestamps are whole seconds -- acquiring "concurrently" at real time risks
# landing in the same second as pass.sh's start_epoch, which the safety
# check's inclusive ">=" correctly (per spec) treats as "ours". A fixed past
# epoch keeps this test deterministic instead of occasionally exercising the
# reclaim path it exists to rule out.
inner_safe="$fixture/inner-lease-safety"
mkdir -p "$inner_safe/ostrom/roles"
printf '{}\n' >"$inner_safe/ostrom/roles/builder.settings.json"
: >"$inner_safe/ostrom/loop-armed"

CLAUDE_CONFIG_DIR="$inner_safe" MANDATE_LEASE_NAME=builder.lease \
  MANDATE_LEASE_NOW_EPOCH=1000 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-othersession-wake3 \
  100000 >/dev/null
preexisting_inner_lease="$(
  CLAUDE_CONFIG_DIR="$inner_safe" MANDATE_LEASE_NAME=builder.lease \
    bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"

CLAUDE_CONFIG_DIR="$inner_safe" CLAUDE_BIN="$fake_claude" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder \
  >"$inner_safe/pass.out" 2>"$inner_safe/pass.err"

[ ! -e "$inner_safe/ostrom/builder-pass.lease" ]
inner_lease_after="$(
  CLAUDE_CONFIG_DIR="$inner_safe" MANDATE_LEASE_NAME=builder.lease \
    bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"
[ "$inner_lease_after" = "$preexisting_inner_lease" ]
grep -q 'leaving it to its own owner' "$inner_safe/pass.err"

# The same pre-existing, concurrently-held inner lease that proves reclaim
# must not touch it also makes the no-op reason knowable: the child found
# its own protocol lease already contended, so this is "lease-held", not
# the generic "blocked" a no-op with no diagnosable cause would carry.
jq -s -e '
  length == 2
  and .[1].kind == "pass-ended"
  and .[1].fact.outcome == "no-op"
  and .[1].fact.reason == "lease-held"
' "$inner_safe/ostrom/sprint.jsonl" >/dev/null

CLAUDE_CONFIG_DIR="$inner_safe" MANDATE_LEASE_NAME=builder.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release builder-othersession-wake3

lease_expiry="$fixture/lease-expiry"
CLAUDE_CONFIG_DIR="$lease_expiry" MANDATE_LEASE_NOW_EPOCH=200 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-alpha 10 >/dev/null
lease_before="$(
  CLAUDE_CONFIG_DIR="$lease_expiry" bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"
set +e
CLAUDE_CONFIG_DIR="$lease_expiry" MANDATE_LEASE_NOW_EPOCH=209 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-beta 10 >/dev/null 2>&1
unexpired_status=$?
set -e
[ "$unexpired_status" -ne 0 ]
lease_after_unexpired="$(
  CLAUDE_CONFIG_DIR="$lease_expiry" bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"
[ "$lease_before" = "$lease_after_unexpired" ]
CLAUDE_CONFIG_DIR="$lease_expiry" MANDATE_LEASE_NOW_EPOCH=210 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-beta 10 >/dev/null
jq -e '
  .owner == "builder-beta"
  and .started_at == 210
  and .expires_at == 220
' <<<"$(
  CLAUDE_CONFIG_DIR="$lease_expiry" bash "$PLUGIN_ROOT/scripts/lease.sh" status
)" >/dev/null

lease_release="$fixture/lease-release"
CLAUDE_CONFIG_DIR="$lease_release" MANDATE_LEASE_NOW_EPOCH=300 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire builder-alpha 10 >/dev/null
release_before="$(
  CLAUDE_CONFIG_DIR="$lease_release" bash "$PLUGIN_ROOT/scripts/lease.sh" status
)"
set +e
CLAUDE_CONFIG_DIR="$lease_release" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release builder-beta >/dev/null 2>&1
non_owner_status=$?
set -e
[ "$non_owner_status" -ne 0 ]
[ "$release_before" = "$(
  CLAUDE_CONFIG_DIR="$lease_release" bash "$PLUGIN_ROOT/scripts/lease.sh" status
)" ]
CLAUDE_CONFIG_DIR="$lease_release" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release builder-alpha
[ ! -e "$lease_release/ostrom/sprint.lease" ]
[ ! -e "$lease_release/ostrom/.sprint.lease.guard" ]

# Daily spend cap (#80): pass.sh sums cost_usd out of today's pass-ended
# records, across every role, and stands down instead of spawning a child
# once that total is at or above the cap -- unbounded spend while the
# principal is asleep is the one loop failure nothing else can undo.
cap_fixture="$fixture/daily-cap"
cap_config="$cap_fixture/config"
mkdir -p "$cap_config/ostrom/roles"
printf '{}\n' >"$cap_config/ostrom/roles/builder.settings.json"
: >"$cap_config/ostrom/loop-armed"
cap_trace="$cap_config/ostrom/sprint.jsonl"
cap_today_epoch=1786449600 # 2026-08-11T12:00:00Z

# Yesterday's row alone is large enough to trip even the default cap, so if
# it were ever misattributed to today the "under cap" run just below would
# wrongly stand down instead of spawning.
MANDATE_TRACE_TIME="2026-08-10T23:59:00Z" CLAUDE_CONFIG_DIR="$cap_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake0","outcome":"completed","cost_usd":71,"duration_seconds":300}' \
    '{}' >/dev/null
# A malformed cost_usd (wrong type) and a missing one must both count as 0,
# not abort the sum -- a gatekeeper row proves the sum is role-blind too.
MANDATE_TRACE_TIME="2026-08-11T00:05:00Z" CLAUDE_CONFIG_DIR="$cap_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"gatekeeper-fixture-wake0","outcome":"completed","cost_usd":"oops","duration_seconds":300}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-11T00:06:00Z" CLAUDE_CONFIG_DIR="$cap_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"gatekeeper-fixture-wake1","outcome":"completed","duration_seconds":300}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-11T00:07:00Z" CLAUDE_CONFIG_DIR="$cap_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake1","outcome":"completed","cost_usd":10,"duration_seconds":300}' \
    '{}' >/dev/null

# Today's well-formed total is 10; the default $50 cap leaves headroom, so
# this pass spawns and runs to completion exactly as it would with no cap
# in play at all.
cap_undercap_args="$cap_fixture/undercap-args"
CLAUDE_CONFIG_DIR="$cap_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  FAKE_CLAUDE_ARGS_FILE="$cap_undercap_args" \
  FAKE_CLAUDE_INNER_OWNER="builder-inner-cap-wake1" \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ -s "$cap_undercap_args" ]
jq -s -e '
  length == 7
  and .[4].kind == "pass-started"
  and .[5].kind == "pass-started"
  and .[6].kind == "pass-ended"
  and .[6].fact.outcome == "completed"
  and .[6].fact.cost_usd == 1.25
' "$cap_trace" >/dev/null

# #99: pass.sh derives "today" from MANDATE_NOW_EPOCH (the simulated day
# above) to sum the cap against, but before this fix it appended its own
# pass-started/pass-ended rows with no MANDATE_TRACE_TIME, so trace.sh
# stamped them with the real wall clock instead -- the two clocks agreed
# only by coincidence, while the real date happened to match the fixture's
# simulated day. This is the regression test that would have caught it: the
# wrapper's own two rows from the run just above must both carry a ts on the
# simulated day (2026-08-11), never on whatever day the machine clock
# actually reads.
jq -s -e '
  (.[4].ts | startswith("2026-08-11"))
  and (.[6].ts | startswith("2026-08-11"))
' "$cap_trace" >/dev/null

# Today's well-formed total is now 10 + 1.25 = 11.25. MANDATE_DAILY_CAP_USD
# overrides the $50 default down to exactly that total: an at-or-over-cap
# pass must record no-op/daily-cap and never spawn a child, and it must
# leave neither the outer role-pass lease nor the inner protocol lease held
# -- the whole point of checking after both lease paths are settled.
cap_overcap_args="$cap_fixture/overcap-args"
CLAUDE_CONFIG_DIR="$cap_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" MANDATE_DAILY_CAP_USD=11.25 \
  FAKE_CLAUDE_ARGS_FILE="$cap_overcap_args" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$cap_overcap_args" ]
[ ! -e "$cap_config/ostrom/builder-pass.lease" ]
[ ! -e "$cap_config/ostrom/builder.lease" ]
jq -s -e '
  length == 9
  and .[7].kind == "pass-started"
  and .[8].kind == "pass-ended"
  and .[8].fact.owner == .[7].fact.owner
  and .[8].fact.outcome == "no-op"
  and .[8].fact.reason == "daily-cap"
  and .[8].fact.cost_usd == null
' "$cap_trace" >/dev/null

# Yesterday's cost must never count toward today's total, tested in
# isolation from the fixture above: a lone $999 row dated yesterday, a
# $50 default cap, and today otherwise empty -- if the day boundary were
# off by even a UTC comparison quirk, this pass would wrongly stand down.
cap_yesterday="$fixture/daily-cap-yesterday"
cap_yesterday_config="$cap_yesterday/config"
mkdir -p "$cap_yesterday_config/ostrom/roles"
printf '{}\n' >"$cap_yesterday_config/ostrom/roles/builder.settings.json"
: >"$cap_yesterday_config/ostrom/loop-armed"
MANDATE_TRACE_TIME="2026-08-10T23:59:59Z" CLAUDE_CONFIG_DIR="$cap_yesterday_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake0","outcome":"completed","cost_usd":999,"duration_seconds":300}' \
    '{}' >/dev/null
cap_yesterday_args="$cap_yesterday/args"
CLAUDE_CONFIG_DIR="$cap_yesterday_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  FAKE_CLAUDE_ARGS_FILE="$cap_yesterday_args" \
  FAKE_CLAUDE_INNER_OWNER="builder-inner-yesterday-wake1" \
  FAKE_CLAUDE_TRACE_SH="$fake_claude_trace_sh" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ -s "$cap_yesterday_args" ]
jq -s -e '
  length == 4
  and .[3].kind == "pass-ended"
  and .[3].fact.outcome == "completed"
' "$cap_yesterday_config/ostrom/sprint.jsonl" >/dev/null

# A malformed or missing cost_usd counts as exactly 0, isolated on its own
# boundary: three bad rows plus one well-formed $7 row put today's true
# total at 7. A cap of 7 must still trip (proving the bad rows did not
# silently drop the day's total below the real one), and a cap of 8 must
# not (proving they did not silently inflate it either).
cap_malformed="$fixture/daily-cap-malformed"
cap_malformed_config="$cap_malformed/config"
mkdir -p "$cap_malformed_config/ostrom/roles"
printf '{}\n' >"$cap_malformed_config/ostrom/roles/builder.settings.json"
: >"$cap_malformed_config/ostrom/loop-armed"
MANDATE_TRACE_TIME="2026-08-11T01:00:00Z" CLAUDE_CONFIG_DIR="$cap_malformed_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake0","outcome":"completed","cost_usd":"oops","duration_seconds":300}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-11T01:01:00Z" CLAUDE_CONFIG_DIR="$cap_malformed_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake1","outcome":"completed","cost_usd":null,"duration_seconds":300}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-11T01:02:00Z" CLAUDE_CONFIG_DIR="$cap_malformed_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake2","outcome":"completed","duration_seconds":300}' \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-11T01:03:00Z" CLAUDE_CONFIG_DIR="$cap_malformed_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake3","outcome":"completed","cost_usd":7,"duration_seconds":300}' \
    '{}' >/dev/null

cap_malformed_trip_args="$cap_malformed/trip-args"
CLAUDE_CONFIG_DIR="$cap_malformed_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" MANDATE_DAILY_CAP_USD=7 \
  FAKE_CLAUDE_ARGS_FILE="$cap_malformed_trip_args" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$cap_malformed_trip_args" ]
jq -s -e '
  length == 6
  and .[5].kind == "pass-ended"
  and .[5].fact.outcome == "no-op"
  and .[5].fact.reason == "daily-cap"
' "$cap_malformed_config/ostrom/sprint.jsonl" >/dev/null

cap_malformed_spawn_args="$cap_malformed/spawn-args"
CLAUDE_CONFIG_DIR="$cap_malformed_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" MANDATE_DAILY_CAP_USD=8 \
  FAKE_CLAUDE_ARGS_FILE="$cap_malformed_spawn_args" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ -s "$cap_malformed_spawn_args" ]

# An unparseable override (not a plain number) falls back to the $50
# default rather than leaving the loops uncapped on a typo.
cap_bad_override="$fixture/daily-cap-bad-override"
cap_bad_override_config="$cap_bad_override/config"
mkdir -p "$cap_bad_override_config/ostrom/roles"
printf '{}\n' >"$cap_bad_override_config/ostrom/roles/builder.settings.json"
: >"$cap_bad_override_config/ostrom/loop-armed"
MANDATE_TRACE_TIME="2026-08-11T01:00:00Z" CLAUDE_CONFIG_DIR="$cap_bad_override_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake0","outcome":"completed","cost_usd":9,"duration_seconds":300}' \
    '{}' >/dev/null
cap_bad_override_args="$cap_bad_override/args"
CLAUDE_CONFIG_DIR="$cap_bad_override_config" CLAUDE_BIN="$fake_claude" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" MANDATE_DAILY_CAP_USD="not-a-number" \
  FAKE_CLAUDE_ARGS_FILE="$cap_bad_override_args" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ -s "$cap_bad_override_args" ]

# #104: triage writes a versioned, canonical order and hands only its path to
# the backend-neutral dispatch verb. The systemd backend returns immediately,
# leaving a per-item lease and work-dispatched row owned by the outliving unit.
dispatch_fixture="$fixture/dispatch"
dispatch_config="$dispatch_fixture/config"
dispatch_bin="$dispatch_fixture/bin"
mkdir -p "$dispatch_config/ostrom" "$dispatch_bin"
dispatch_candidate="$dispatch_fixture/candidate.json"
cat >"$dispatch_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#123","repository":"example-org/example-repo","item_ref":"#123","branch_name":"feat/123-placeholder","spec":"Implement one synthetic behavior.","acceptance_criteria":["The synthetic behavior is observable."],"constraints":["Use placeholder data only."]}
JSON
dispatch_order="$(
  CLAUDE_CONFIG_DIR="$dispatch_config" \
    MANDATE_TRACE_TIME="2026-08-11T02:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$dispatch_candidate" \
      2>"$dispatch_fixture/branch-overwrite.err"
)"
bash "$PLUGIN_ROOT/scripts/work-order.sh" validate "$dispatch_order"
dispatch_branch="$(jq -r '.branch_name' "$dispatch_order")"
[ "$dispatch_branch" = "$(
  bash "$PLUGIN_ROOT/scripts/work-order.sh" branch-name \
    'example-org/example-repo#123'
)" ]
[ "$dispatch_branch" = 'ostrom/123-9bb890b1b3b4' ]
grep -Fq "overwriting candidate branch_name 'feat/123-placeholder' with item-derived '$dispatch_branch'" \
  "$dispatch_fixture/branch-overwrite.err"

# #142: branch naming is an item identity property, not fresh candidate prose.
# Replacing the same item's order with different free text must produce the
# same branch even though the order id and creation time change.
first_dispatch_branch="$dispatch_branch"
jq '.branch_name = "reworded/completely-different"' \
  "$dispatch_candidate" >"$dispatch_fixture/reworded-candidate.json"
dispatch_order="$({
  CLAUDE_CONFIG_DIR="$dispatch_config" \
    MANDATE_TRACE_TIME="2026-08-11T02:00:01Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create \
      "$dispatch_fixture/reworded-candidate.json"
} 2>"$dispatch_fixture/reworded-branch-overwrite.err")"
[ "$(jq -r '.branch_name' "$dispatch_order")" = "$first_dispatch_branch" ]
grep -Fq "overwriting candidate branch_name 'reworded/completely-different' with item-derived '$first_dispatch_branch'" \
  "$dispatch_fixture/reworded-branch-overwrite.err"

# Compatibility is validation-only: an order written before deterministic
# naming landed still satisfies the unchanged schema_version 1 contract.
jq '.branch_name = "feat/123-historical"' "$dispatch_order" \
  >"$dispatch_fixture/historical-order.json"
bash "$PLUGIN_ROOT/scripts/work-order.sh" validate \
  "$dispatch_fixture/historical-order.json"
jq -e '
  keys == ["acceptance_criteria", "branch_name", "constraints",
    "cost_ceiling_usd", "created_at", "item_id", "item_ref", "order_id",
    "repository", "schema_version", "spec", "token_ceiling"]
  and .schema_version == 1
  and .cost_ceiling_usd == 20
  and .token_ceiling == 500000
  and .item_id == "example-org/example-repo#123"
  and .branch_name == "ostrom/123-9bb890b1b3b4"
' "$dispatch_order" >/dev/null

fake_dispatch_gh="$dispatch_bin/gh-as"
cat >"$fake_dispatch_gh" <<'SH'
#!/usr/bin/env bash
if [ -n "${FAKE_GH_CALLS:-}" ]; then
  printf '%s\n' "$*" >>"$FAKE_GH_CALLS"
fi
if [ "$4" = api ] && [ "$5" = --paginate ] && [ "$6" = --slurp ]; then
  if [ "${FAKE_REMOTE_QUERY_FAIL:-0}" -eq 1 ]; then
    exit 42
  fi
  if [ -n "${FAKE_REMOTE_BRANCH_NAME:-}" ]; then
    jq -cn \
      --arg main_sha "${FAKE_DEFAULT_SHA:-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}" \
      --arg branch_name "$FAKE_REMOTE_BRANCH_NAME" \
      --arg branch_sha "${FAKE_REMOTE_BRANCH_SHA:-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb}" \
      '[[{name:"main",commit:{sha:$main_sha}},
        {name:$branch_name,commit:{sha:$branch_sha}}]]'
  else
    printf '%s\n' '[[{"name":"main","commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]]'
  fi
  exit 0
fi
if [ "$4" = repo ] && [ "$5" = view ]; then
  printf '%s\n' main
  exit 0
fi
if [ "$4" = api ] && [[ "$5" == repos/*/compare/* ]]; then
  printf '%s\n' "${FAKE_REMOTE_AHEAD:-0}"
  exit 0
fi
if [ "$4" = pr ] && [ "$5" = list ]; then
  if [ "${FAKE_OPEN_PR:-0}" -eq 1 ]; then
    printf '%s\n' '[{"number":77,"body":"Closes example-org/example-repo#123","url":"https://example.test/pull/77"}]'
  else
    printf '%s\n' '[]'
  fi
  exit 0
fi
if [ "${FAKE_OPEN_PR:-0}" -ne 1 ]; then
  printf '%s\n' '[]'
fi
SH
fake_systemd_run="$dispatch_bin/systemd-run"
cat >"$fake_systemd_run" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_SYSTEMD_ARGS"
printf 'called\n' >>"$FAKE_SYSTEMD_CALLS"
SH
fake_dispatch_codex="$dispatch_bin/codex"
cat >"$fake_dispatch_codex" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$fake_dispatch_gh" "$fake_systemd_run" "$fake_dispatch_codex"
dispatch_args="$dispatch_fixture/systemd-args"
dispatch_calls="$dispatch_fixture/systemd-calls"
: >"$dispatch_calls"

# #153: a remote branch is durable work even without a pull request. The
# branch read, default lookup, and comparison all use the builder wrapper;
# rejection precedes the lease and the work-dispatched reservation.
branch_guard_config="$dispatch_fixture/branch-guard-config"
branch_guard_gh_calls="$dispatch_fixture/branch-guard-gh-calls"
branch_guard_stderr="$dispatch_fixture/branch-guard.err"
mkdir -p "$branch_guard_config/ostrom"
set +e
FAKE_REMOTE_BRANCH_NAME="$dispatch_branch" \
  FAKE_REMOTE_BRANCH_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
  FAKE_REMOTE_AHEAD=4 FAKE_GH_CALLS="$branch_guard_gh_calls" \
  CLAUDE_CONFIG_DIR="$branch_guard_config" \
  MANDATE_TRACE_TIME="2026-08-11T02:00:30Z" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" \
    >"$dispatch_fixture/branch-guard.out" 2>"$branch_guard_stderr"
branch_guard_status=$?
set -e
if [ "$branch_guard_status" -ne 3 ]; then
  echo "remote branch guard did not exit 3" >&2
  exit 1
fi
branch_guard_item_hash="$(
  bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash \
    'example-org/example-repo#123'
)"
if [ -e "$branch_guard_config/ostrom/implementer-item-$branch_guard_item_hash.lease" ]; then
  echo "remote branch guard leaked an implementer lease" >&2
  exit 1
fi
if [ -s "$dispatch_calls" ]; then
  echo "remote branch guard launched a systemd unit" >&2
  exit 1
fi
if jq -s -e 'any(.[]; .kind == "work-dispatched")' \
  "$branch_guard_config/ostrom/sprint.jsonl" >/dev/null; then
  echo "remote branch guard reserved concurrency or cost" >&2
  exit 1
fi
if jq -s -e --arg branch "$dispatch_branch" '
  length != 1
  or .[0].kind != "work-failed"
  or .[0].fact.reason != "branch-already-pushed"
  or .[0].fact.repository != "example-org/example-repo"
  or .[0].fact.branch_name != $branch
  or .[0].fact.head_sha != "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  or .[0].fact.ahead_of_default != 4
' "$branch_guard_config/ostrom/sprint.jsonl" >/dev/null; then
  echo "remote branch guard did not record the required work-failed detail" >&2
  exit 1
fi
expected_branch_guard_error="repository=example-org/example-repo branch=$dispatch_branch head=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ahead=4"
if [ "$(grep -Fc "$expected_branch_guard_error" "$branch_guard_stderr")" -ne 1 ]; then
  echo "remote branch guard stderr omitted branch detail" >&2
  exit 1
fi
if [ "$(grep -Fc 'builder example-org/example-repo gh api --paginate --slurp repos/example-org/example-repo/branches?per_page=100' "$branch_guard_gh_calls")" -ne 1 ]; then
  echo "remote branch guard did not query branches through the builder wrapper" >&2
  exit 1
fi
if [ "$(grep -c '^builder example-org/example-repo gh ' "$branch_guard_gh_calls")" -ne "$(wc -l <"$branch_guard_gh_calls" | tr -d '[:space:]')" ]; then
  echo "remote branch guard made a remote read outside the builder wrapper" >&2
  exit 1
fi

# A branch with the item number at the start of a path component matches the
# deterministic ostrom/<number>-... position even when its wording predates
# the order's derived branch name.
set +e
FAKE_REMOTE_BRANCH_NAME='chore/123-existing-work' \
  FAKE_REMOTE_BRANCH_SHA=cccccccccccccccccccccccccccccccccccccccc \
  FAKE_REMOTE_AHEAD=2 CLAUDE_CONFIG_DIR="$branch_guard_config" \
  MANDATE_TRACE_TIME="2026-08-11T02:00:31Z" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" >/dev/null 2>&1
numbered_branch_guard_status=$?
set -e
if [ "$numbered_branch_guard_status" -ne 3 ]; then
  echo "number-position remote branch guard did not exit 3" >&2
  exit 1
fi
if [ "$(jq -s -r 'last.fact.branch_name' "$branch_guard_config/ostrom/sprint.jsonl")" != 'chore/123-existing-work' ]; then
  echo "number-position remote branch guard recorded the wrong branch" >&2
  exit 1
fi
if jq -s -e 'any(.[]; .kind == "work-dispatched")' \
  "$branch_guard_config/ostrom/sprint.jsonl" >/dev/null; then
  echo "number-position remote branch guard reserved work" >&2
  exit 1
fi

# Failure to enumerate remote branches fails closed before any reservation.
branch_query_failure_config="$dispatch_fixture/branch-query-failure-config"
branch_query_failure_stderr="$dispatch_fixture/branch-query-failure.err"
mkdir -p "$branch_query_failure_config/ostrom"
set +e
FAKE_REMOTE_QUERY_FAIL=1 CLAUDE_CONFIG_DIR="$branch_query_failure_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" \
    >/dev/null 2>"$branch_query_failure_stderr"
branch_query_failure_status=$?
set -e
if [ "$branch_query_failure_status" -eq 0 ]; then
  echo "remote branch query failure dispatched blind" >&2
  exit 1
fi
if [ -e "$branch_query_failure_config/ostrom/implementer-item-$branch_guard_item_hash.lease" ]; then
  echo "remote branch query failure leaked an implementer lease" >&2
  exit 1
fi
if [ -e "$branch_query_failure_config/ostrom/sprint.jsonl" ]; then
  echo "remote branch query failure reserved work" >&2
  exit 1
fi
if [ "$(grep -Fc 'could not verify remote branches for example-org/example-repo#123 in example-org/example-repo' "$branch_query_failure_stderr")" -ne 1 ]; then
  echo "remote branch query failure was not reported" >&2
  exit 1
fi

clean_dispatch_gh_calls="$dispatch_fixture/clean-dispatch-gh-calls"
dispatch_unit="$(
  CLAUDE_CONFIG_DIR="$dispatch_config" \
    MANDATE_TRACE_TIME="2026-08-11T02:01:00Z" \
    MANDATE_NOW_EPOCH="$cap_today_epoch" \
    MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
    MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
    CODEX_BIN="$fake_dispatch_codex" \
    FAKE_GH_CALLS="$clean_dispatch_gh_calls" \
    FAKE_SYSTEMD_ARGS="$dispatch_args" \
    FAKE_SYSTEMD_CALLS="$dispatch_calls" \
    bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order"
)"
if [ "$(grep -Fc 'builder example-org/example-repo gh api --paginate --slurp repos/example-org/example-repo/branches?per_page=100' "$clean_dispatch_gh_calls")" -ne 1 ]; then
  echo "clean dispatch did not perform the remote branch preflight" >&2
  exit 1
fi
if [ "$dispatch_unit" != "ostrom-implementer-9bb890b1b3b47926" ]; then
  echo "clean remote branch preflight changed successful dispatch" >&2
  exit 1
fi
[ "$dispatch_unit" = "ostrom-implementer-9bb890b1b3b47926" ]
[ "$(wc -l <"$dispatch_calls" | tr -d '[:space:]')" -eq 1 ]
grep -qx 'RuntimeMaxSec=infinity' "$dispatch_args"
grep -qx 'KillMode=control-group' "$dispatch_args"
grep -qx "$PLUGIN_ROOT/scripts/implement.sh" "$dispatch_args"
dispatch_lease="$dispatch_config/ostrom/implementer-item-9bb890b1b3b47926c7fff1a3e7e38413f8ad65bea2a5b126acc3c6098fd7fb82.lease"
jq -e --arg owner "$dispatch_unit" '.owner == $owner' "$dispatch_lease" >/dev/null
jq -s -e '
  length == 1
  and .[0].kind == "work-dispatched"
  and .[0].fact.schema_version == 1
  and .[0].fact.item_id == "example-org/example-repo#123"
  and .[0].fact.unit_name == "ostrom-implementer-9bb890b1b3b47926"
  and .[0].fact.backend == "systemd"
  and .[0].fact.cost_usd == null
  and .[0].fact.duration_seconds == 0
' "$dispatch_config/ostrom/sprint.jsonl" >/dev/null

# The three pre-existing duplicate guards still fail closed independently. A
# live lease refuses first; after its controlled release, the unmatched
# dispatch row refuses; after a terminal row, an open PR reference refuses.
set +e
CLAUDE_CONFIG_DIR="$dispatch_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" >/dev/null 2>&1
live_lease_dispatch_status=$?
set -e
[ "$live_lease_dispatch_status" -eq 3 ]
[ "$(wc -l <"$dispatch_calls" | tr -d '[:space:]')" -eq 1 ]
CLAUDE_CONFIG_DIR="$dispatch_config" \
  MANDATE_LEASE_NAME="${dispatch_lease##*/}" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release "$dispatch_unit"
set +e
CLAUDE_CONFIG_DIR="$dispatch_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" >/dev/null 2>&1
inflight_dispatch_status=$?
set -e
[ "$inflight_dispatch_status" -eq 3 ]
dispatch_order_id="$(jq -r '.order_id' "$dispatch_order")"
MANDATE_TRACE_TIME="2026-08-11T02:02:00Z" \
  CLAUDE_CONFIG_DIR="$dispatch_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append work-failed \
    "$(jq -cn --arg order_id "$dispatch_order_id" \
      '{schema_version:1,item_id:"example-org/example-repo#123",
        order_id:$order_id,unit_name:"ostrom-implementer-9bb890b1b3b47926",
        backend:"systemd",cost_ceiling_usd:20,token_ceiling:500000,
        cost_usd:0,duration_seconds:60,pr_url:null,reason:"synthetic",
        usage:{input_tokens:0,cached_input_tokens:0,output_tokens:0,
          reasoning_output_tokens:0}}')" \
    '{}' >/dev/null
set +e
FAKE_OPEN_PR=1 CLAUDE_CONFIG_DIR="$dispatch_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dispatch_order" >/dev/null 2>&1
open_pr_dispatch_status=$?
set -e
[ "$open_pr_dispatch_status" -eq 3 ]
[ "$(wc -l <"$dispatch_calls" | tr -d '[:space:]')" -eq 1 ]

# Two unmatched orders consume the default concurrency limit even if they are
# for different items. The new item's speculative lease is released when the
# dispatcher refuses it.
concurrency_config="$dispatch_fixture/concurrency-config"
mkdir -p "$concurrency_config/ostrom"
cat >"$concurrency_config/ostrom/sprint.jsonl" <<'JSONL'
{"ts":"2026-08-11T01:00:00Z","kind":"work-dispatched","fact":{"schema_version":1,"item_id":"example-org/example-repo#130","order_id":"order-a","unit_name":"ostrom-implementer-aaaaaaaaaaaaaaaa","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":0},"narration":{}}
{"ts":"2026-08-11T01:01:00Z","kind":"work-dispatched","fact":{"schema_version":1,"item_id":"example-org/example-repo#131","order_id":"order-b","unit_name":"ostrom-implementer-bbbbbbbbbbbbbbbb","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":0},"narration":{}}
JSONL
concurrency_candidate="$dispatch_fixture/concurrency-candidate.json"
cat >"$concurrency_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#132","repository":"example-org/example-repo","item_ref":"#132","branch_name":"feat/132-placeholder","spec":"Implement a concurrency fixture.","acceptance_criteria":["The fixture is observable."],"constraints":["Use placeholder data only."]}
JSON
concurrency_order="$(
  CLAUDE_CONFIG_DIR="$concurrency_config" \
    MANDATE_TRACE_TIME="2026-08-11T01:02:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$concurrency_candidate"
)"
set +e
CLAUDE_CONFIG_DIR="$concurrency_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_MAX_IMPLEMENTERS=2 \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$concurrency_order" >/dev/null 2>&1
concurrency_dispatch_status=$?
set -e
[ "$concurrency_dispatch_status" -eq 3 ]
concurrency_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#132')"
[ ! -e "$concurrency_config/ostrom/implementer-item-$concurrency_item_hash.lease" ]
[ "$(wc -l <"$dispatch_calls" | tr -d '[:space:]')" -eq 1 ]

# The daily cap is checked again immediately before dispatch, including the
# new order's full reservation. Reaching it creates no unit and leaks no lease.
cap_dispatch_candidate="$dispatch_fixture/cap-candidate.json"
cat >"$cap_dispatch_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#124","repository":"example-org/example-repo","item_ref":"#124","branch_name":"feat/124-placeholder","spec":"Implement another synthetic behavior.","acceptance_criteria":["The other behavior is observable."],"constraints":["Use placeholder data only."]}
JSON
cap_dispatch_order="$(
  CLAUDE_CONFIG_DIR="$dispatch_config" \
    MANDATE_TRACE_TIME="2026-08-11T02:03:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$cap_dispatch_candidate"
)"
MANDATE_TRACE_TIME="2026-08-11T02:04:00Z" \
  CLAUDE_CONFIG_DIR="$dispatch_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-synthetic-wake1","outcome":"completed","cost_usd":50,"duration_seconds":1}' \
    '{}' >/dev/null
set +e
CLAUDE_CONFIG_DIR="$dispatch_config" \
  MANDATE_NOW_EPOCH="$cap_today_epoch" \
  MANDATE_DAILY_CAP_USD=50 \
  MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" \
  CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$dispatch_args" FAKE_SYSTEMD_CALLS="$dispatch_calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$cap_dispatch_order" >/dev/null 2>&1
cap_dispatch_status=$?
set -e
[ "$cap_dispatch_status" -eq 3 ]
cap_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#124')"
[ ! -e "$dispatch_config/ostrom/implementer-item-$cap_item_hash.lease" ]
[ "$(wc -l <"$dispatch_calls" | tr -d '[:space:]')" -eq 1 ]

# The implementer owns cleanup for its whole lifetime. A TERM during Codex
# kills the child, records work-failed, and releases the per-item lease. A
# separate completed order proves the same wrapper commits, pushes, and opens
# its PR after the offline Codex process has exited.
implement_fixture="$fixture/implementer"
implement_config="$implement_fixture/config"
implement_source="$implement_fixture/source"
implement_remote="$implement_fixture/origin.git"
implement_bin="$implement_fixture/bin"
mkdir -p "$implement_config/ostrom" "$implement_source" "$implement_bin"
git -C "$implement_source" init -b main >/dev/null
git -C "$implement_source" config user.name "Ostrom Test"
git -C "$implement_source" config user.email "ostrom@example.test"
printf 'base\n' >"$implement_source/base.txt"
git -C "$implement_source" add base.txt
git -C "$implement_source" commit -m fixture >/dev/null
git init --bare "$implement_remote" >/dev/null
git -C "$implement_source" remote add origin "$implement_remote"
git -C "$implement_source" push -u origin main >/dev/null

fake_implement_gh="$implement_bin/gh-as"
cat >"$fake_implement_gh" <<'SH'
#!/usr/bin/env bash
shift 2
if [ "$1" = gh ] && [ "$2" = api ] && [ "$3" = --paginate ] && [ "$4" = --slurp ]; then
  printf '%s\n' '[[{"name":"main","commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]]'
  exit 0
fi
if [ "$1" = gh ] && [ "$2" = repo ] && [ "$3" = view ]; then
  printf 'main\n'
  exit 0
fi
if [ "$1" = gh ] && [ "$2" = pr ] && [ "$3" = list ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = gh ] && [ "$2" = pr ] && [ "$3" = create ]; then
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --body-file ]; then
      cp "$2" "$FAKE_PR_BODY"
      break
    fi
    shift
  done
  printf 'https://example.test/pull/125\n'
  exit 0
fi
args=("$@")
for index in "${!args[@]}"; do
  case "${args[$index]}" in
    https://github.com/*.git) args[$index]="$FAKE_GIT_REMOTE" ;;
  esac
done
exec "${args[@]}"
SH
fake_codex="$implement_bin/codex"
cat >"$fake_codex" <<'SH'
#!/usr/bin/env bash
usage_error() {
  printf "error: unexpected argument '%s' found\n\n" "$1" >&2
  printf 'Usage: codex exec [OPTIONS] [PROMPT]\n' >&2
  exit 2
}

config_error() {
  printf '%s\n' \
    'Error loading config.toml: data did not match any variant of untagged enum FeatureToml' >&2
  exit 1
}

validate_config() {
  # Keep this payload allow-list aligned with the pinned CLI. In particular,
  # it intentionally accepts no features.* override: rollout_budget cannot be
  # configured through -c because its required reminder value is rejected.
  case "$1" in
    'approval_policy="never"'|\
      'sandbox_workspace_write.network_access=false'|\
      'web_search="disabled"') ;;
    *) config_error ;;
  esac
}

[ "${1:-}" = exec ] || usage_error "${1:-}"
shift
worktree=""
result=""
while [ "$#" -gt 0 ]; do
  # Keep this accepted option set aligned with the pinned codex exec CLI.
  # Unknown options deliberately fail like clap so wrapper drift breaks CI.
  case "$1" in
    --json) shift ;;
    -C|--cd) worktree="$2"; shift 2 ;;
    -s|--sandbox) shift 2 ;;
    -c|--config) validate_config "$2"; shift 2 ;;
    -o|--output-last-message) result="$2"; shift 2 ;;
    --) shift; break ;;
    -*) usage_error "$1" ;;
    *) shift ;;
  esac
done
case "${FAKE_CODEX_MODE:-complete}" in
  wait)
    printf '%s\n' "$$" >"$FAKE_CODEX_MARKER"
    trap 'exit 143' TERM
    while :; do sleep 1; done
    ;;
  usage)
    usage_error '--removed-flag'
    ;;
  config)
    config_error
    ;;
  config-required)
    printf '%s\n' \
      'Error: features.rollout_budget.reminder_at_remaining_tokens is required when rollout_budget is enabled' >&2
    exit 1
    ;;
  model-failure)
    printf 'Synthetic model failure.\n' >&2
    exit 1
    ;;
  partial-failure)
    printf 'partial implementation\n' >"$worktree/generated.txt"
    printf '%s\n' \
      '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"reasoning_output_tokens":10}}'
    printf 'Synthetic model failure after changing the worktree.\n' >&2
    exit 1
    ;;
  ceiling)
    printf 'partial implementation\n' >"$worktree/generated.txt"
    printf '%s\n' "$$" >"$FAKE_CODEX_MARKER"
    trap 'exit 143' TERM
    printf '%s\n' "$FAKE_CODEX_USAGE_JSON"
    for _ceiling_wait in $(seq 1 100); do
      sleep 0.02
    done
    printf 'not terminated\n' >"$FAKE_CODEX_SURVIVED_MARKER"
    ;;
  complete)
    printf 'implemented\n' >"$worktree/generated.txt"
    printf 'Synthetic implementation completed.\n' >"$result"
    fake_usage_json="${FAKE_CODEX_USAGE_JSON:-}"
    if [ -z "$fake_usage_json" ]; then
      fake_usage_json='{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"reasoning_output_tokens":10}}'
    fi
    printf '%s\n' "$fake_usage_json"
    ;;
esac
SH
chmod +x "$fake_implement_gh" "$fake_codex"

create_implement_order() {
  local case_name="$1"
  local item_number="$2"
  local order_config="${3:-$implement_config}"
  local candidate_file="$implement_fixture/$case_name-candidate.json"
  cat >"$candidate_file" <<JSON
{"schema_version":1,"item_id":"example-org/example-repo#$item_number","repository":"example-org/example-repo","item_ref":"#$item_number","branch_name":"candidate/$case_name","spec":"Exercise synthetic worktree reuse.","acceptance_criteria":["The existing worktree is handled safely."],"constraints":["Use placeholder data only."]}
JSON
  CLAUDE_CONFIG_DIR="$order_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:00:10Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$candidate_file" \
      2>/dev/null
}

run_implement_order() {
  local order_file="$1"
  local case_name="$2"
  local runtime_config="${3:-$implement_config}"
  local item_id item_hash unit lease
  item_id="$(jq -r '.item_id' "$order_file")"
  item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash "$item_id")"
  unit="ostrom-implementer-${item_hash:0:16}"
  lease="implementer-item-$item_hash.lease"
  CLAUDE_CONFIG_DIR="$runtime_config" MANDATE_LEASE_NAME="$lease" \
    bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$unit" 3600 >/dev/null
  FAKE_CODEX_MODE=complete CODEX_BIN="$fake_codex" \
    CLAUDE_CONFIG_DIR="$runtime_config" \
    MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
    MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
    FAKE_PR_BODY="$implement_fixture/$case_name-pr-body" \
    bash "$PLUGIN_ROOT/scripts/implement.sh" "$order_file" "$unit" \
      >"$implement_fixture/$case_name.out" \
      2>"$implement_fixture/$case_name.err"
}

# #142: a repeat order on the already-matching deterministic branch reuses the
# item-keyed worktree. The preexisting uncommitted file reaches the pushed
# commit, proving reuse does not discard it.
reuse_config="$implement_fixture/reuse-config"
reuse_order="$(create_implement_order reuse 140 "$reuse_config")"
reuse_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#140')"
reuse_branch="$(jq -r '.branch_name' "$reuse_order")"
reuse_worktree="$reuse_config/ostrom/implementer-worktrees/$reuse_item_hash"
mkdir -p "$(dirname "$reuse_worktree")"
git -C "$implement_source" worktree add -b "$reuse_branch" \
  "$reuse_worktree" refs/remotes/origin/main >/dev/null
printf 'preserved before redispatch\n' >"$reuse_worktree/preserved.txt"
run_implement_order "$reuse_order" reuse "$reuse_config"
[ "$(git -C "$reuse_worktree" branch --show-current)" = "$reuse_branch" ]
[ "$(git --git-dir="$implement_remote" show "$reuse_branch:preserved.txt")" = \
  'preserved before redispatch' ]

# A historical order can target a different branch than a clean, not-ahead
# item worktree. The implementer retargets it after fetch and proceeds.
retarget_config="$implement_fixture/retarget-config"
retarget_order="$(create_implement_order retarget 141 "$retarget_config")"
retarget_branch='fix/141-historical-order'
jq --arg branch "$retarget_branch" '.branch_name = $branch' \
  "$retarget_order" >"$implement_fixture/retarget-historical-order.json"
retarget_order="$implement_fixture/retarget-historical-order.json"
bash "$PLUGIN_ROOT/scripts/work-order.sh" validate "$retarget_order"
retarget_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#141')"
retarget_worktree="$retarget_config/ostrom/implementer-worktrees/$retarget_item_hash"
git -C "$implement_source" worktree add -b old/141-reworded \
  "$retarget_worktree" refs/remotes/origin/main >/dev/null
run_implement_order "$retarget_order" retarget "$retarget_config"
[ "$(git -C "$retarget_worktree" branch --show-current)" = "$retarget_branch" ]
git --git-dir="$implement_remote" show-ref --verify \
  "refs/heads/$retarget_branch" >/dev/null

# A dirty mismatch is unsatisfiable without human choice. Dispatch records the
# exact worktree and existing branch before acquiring a lease, consuming a
# concurrency slot, reserving cost, querying GitHub, or launching systemd.
dirty_config="$implement_fixture/dirty-preflight-config"
mkdir -p "$dirty_config/ostrom/implementer-worktrees"
dirty_order="$(create_implement_order dirty-preflight 142 "$dirty_config")"
dirty_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#142')"
dirty_branch="$(jq -r '.branch_name' "$dirty_order")"
dirty_worktree="$dirty_config/ostrom/implementer-worktrees/$dirty_item_hash"
git -C "$implement_source" worktree add -b old/142-stranded \
  "$dirty_worktree" refs/remotes/origin/main >/dev/null
printf 'must survive\n' >"$dirty_worktree/preserved.txt"
cat >"$dirty_config/ostrom/sprint.jsonl" <<'JSONL'
{"ts":"2026-08-11T01:00:00Z","kind":"work-dispatched","fact":{"schema_version":1,"item_id":"example-org/example-repo#999","order_id":"already-inflight","unit_name":"ostrom-implementer-ffffffffffffffff","backend":"systemd","cost_ceiling_usd":20,"token_ceiling":500000,"cost_usd":null,"duration_seconds":0},"narration":{}}
{"ts":"2026-08-11T01:01:00Z","kind":"pass-ended","fact":{"owner":"builder-fixture-wake0","outcome":"completed","cost_usd":50,"duration_seconds":1},"narration":{}}
JSONL
set +e
CLAUDE_CONFIG_DIR="$dirty_config" MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" CODEX_BIN="$fake_dispatch_codex" \
  MANDATE_MAX_IMPLEMENTERS=1 MANDATE_DAILY_CAP_USD=1 \
  FAKE_SYSTEMD_ARGS="$implement_fixture/dirty-systemd-args" \
  FAKE_SYSTEMD_CALLS="$implement_fixture/dirty-systemd-calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$dirty_order" \
    >"$implement_fixture/dirty.out" 2>"$implement_fixture/dirty.err"
dirty_status=$?
set -e
[ "$dirty_status" -eq 3 ]
[ -f "$dirty_worktree/preserved.txt" ]
[ "$(git -C "$dirty_worktree" branch --show-current)" = old/142-stranded ]
[ ! -e "$dirty_config/ostrom/implementer-item-$dirty_item_hash.lease" ]
[ ! -e "$implement_fixture/dirty-systemd-calls" ]
jq -s -e --arg path "$dirty_worktree" '
  ([.[] | select(.kind == "work-dispatched")] | length) == 1
  and ([.[] | select(.kind == "work-failed"
    and .fact.reason == "worktree-branch-mismatch")] | length) == 1
  and (last | .kind == "work-failed")
  and (last | .fact.worktree_path == $path)
  and (last | .fact.branch_name == "old/142-stranded")
  and (last | .fact.cost_usd == 0)
' "$dirty_config/ostrom/sprint.jsonl" >/dev/null

# Clean working files are not enough when retargeting would abandon commits.
# The same preflight rejects an ahead branch without a lease or launch.
ahead_config="$implement_fixture/ahead-preflight-config"
mkdir -p "$ahead_config/ostrom/work-orders" \
  "$ahead_config/ostrom/implementer-worktrees"
ahead_source="$implement_fixture/ahead-source"
git clone "$implement_remote" "$ahead_source" >/dev/null 2>&1
git -C "$ahead_source" config user.name "Ostrom Test"
git -C "$ahead_source" config user.email "ostrom@example.test"
git -C "$ahead_source" remote set-head origin main
ahead_candidate="$implement_fixture/ahead-candidate.json"
cat >"$ahead_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#143","repository":"example-org/example-repo","item_ref":"#143","branch_name":"candidate/ahead","spec":"Exercise ahead worktree preservation.","acceptance_criteria":["Ahead commits survive."],"constraints":["Use placeholder data only."]}
JSON
ahead_order="$(CLAUDE_CONFIG_DIR="$ahead_config" bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$ahead_candidate" 2>/dev/null)"
ahead_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#143')"
ahead_worktree="$ahead_config/ostrom/implementer-worktrees/$ahead_item_hash"
git -C "$ahead_source" worktree add -b old/143-ahead \
  "$ahead_worktree" refs/remotes/origin/main >/dev/null
printf 'committed work\n' >"$ahead_worktree/ahead.txt"
git -C "$ahead_worktree" add ahead.txt
git -C "$ahead_worktree" commit -m 'fixture ahead work' >/dev/null
set +e
CLAUDE_CONFIG_DIR="$ahead_config" MANDATE_GH_AS_BIN="$fake_dispatch_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_systemd_run" CODEX_BIN="$fake_dispatch_codex" \
  FAKE_SYSTEMD_ARGS="$implement_fixture/ahead-systemd-args" \
  FAKE_SYSTEMD_CALLS="$implement_fixture/ahead-systemd-calls" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$ahead_order" >/dev/null 2>&1
ahead_status=$?
set -e
[ "$ahead_status" -eq 3 ]
[ ! -e "$ahead_config/ostrom/implementer-item-$ahead_item_hash.lease" ]
[ ! -e "$implement_fixture/ahead-systemd-calls" ]
[ "$(git -C "$ahead_worktree" rev-list --count refs/remotes/origin/main..HEAD)" -eq 1 ]

run_implement_usage_case() {
  case_number="$1"
  case_usage="$2"
  case_mode="${3:-complete}"
  case_streaming_ceiling="${4:-enabled}"
  case_candidate="$implement_fixture/$case_number-candidate.json"
  cat >"$case_candidate" <<JSON
{"schema_version":1,"item_id":"example-org/example-repo#$case_number","repository":"example-org/example-repo","item_ref":"#$case_number","branch_name":"fix/$case_number-placeholder","spec":"Exercise synthetic token accounting.","acceptance_criteria":["Token accounting matches the reported components."],"constraints":["Use placeholder data only."]}
JSON
  IMPLEMENT_CASE_ORDER="$(
    CLAUDE_CONFIG_DIR="$implement_config" \
      MANDATE_TRACE_TIME="2026-08-11T03:00:15Z" \
      bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$case_candidate"
  )"
  IMPLEMENT_CASE_ORDER_ID="$(jq -r '.order_id' "$IMPLEMENT_CASE_ORDER")"
  IMPLEMENT_CASE_BRANCH="$(jq -r '.branch_name' "$IMPLEMENT_CASE_ORDER")"
  IMPLEMENT_CASE_ITEM_HASH="$(
    bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash \
      "example-org/example-repo#$case_number"
  )"
  IMPLEMENT_CASE_UNIT="ostrom-implementer-${IMPLEMENT_CASE_ITEM_HASH:0:16}"
  IMPLEMENT_CASE_LEASE="implementer-item-$IMPLEMENT_CASE_ITEM_HASH.lease"
  IMPLEMENT_CASE_WORKTREE="$implement_config/ostrom/implementer-worktrees/$IMPLEMENT_CASE_ITEM_HASH"
  IMPLEMENT_CASE_CODEX_MARKER="$implement_fixture/$case_number-codex-pid"
  IMPLEMENT_CASE_SURVIVED_MARKER="$implement_fixture/$case_number-codex-survived"
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_LEASE_NAME="$IMPLEMENT_CASE_LEASE" \
    bash "$PLUGIN_ROOT/scripts/lease.sh" acquire \
      "$IMPLEMENT_CASE_UNIT" 3600 >/dev/null
  set +e
  FAKE_CODEX_MODE="$case_mode" FAKE_CODEX_USAGE_JSON="$case_usage" \
    FAKE_CODEX_MARKER="$IMPLEMENT_CASE_CODEX_MARKER" \
    FAKE_CODEX_SURVIVED_MARKER="$IMPLEMENT_CASE_SURVIVED_MARKER" \
    MANDATE_IMPLEMENTER_STREAMING_CEILING="$case_streaming_ceiling" \
    CODEX_BIN="$fake_codex" CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
    MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
    FAKE_PR_BODY="$implement_fixture/$case_number-pr-body" \
    bash "$PLUGIN_ROOT/scripts/implement.sh" \
      "$IMPLEMENT_CASE_ORDER" "$IMPLEMENT_CASE_UNIT" \
      >"$implement_fixture/$case_number.out" \
      2>"$implement_fixture/$case_number.err"
  IMPLEMENT_CASE_STATUS=$?
  set -e
  [ ! -e "$implement_config/ostrom/$IMPLEMENT_CASE_LEASE" ]
}

# The fake must reject a recognized flag whose config payload is not on the
# pinned-CLI allow-list, rather than treating every -c argument as valid.
set +e
"$fake_codex" exec -c features.rollout_budget.enabled=true \
  >"$implement_fixture/rejected-config.out" \
  2>"$implement_fixture/rejected-config.err"
rejected_config_status=$?
set -e
[ "$rejected_config_status" -eq 1 ]
grep -q \
  '^Error loading config\.toml: data did not match any variant of untagged enum FeatureToml$' \
  "$implement_fixture/rejected-config.err"

implement_kill_candidate="$implement_fixture/kill-candidate.json"
cat >"$implement_kill_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#125","repository":"example-org/example-repo","item_ref":"#125","branch_name":"feat/125-placeholder","spec":"Implement a synthetic killed order.","acceptance_criteria":["The synthetic change exists."],"constraints":["Use placeholder data only."]}
JSON
implement_kill_order="$(
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$implement_kill_candidate"
)"
implement_kill_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#125')"
implement_kill_unit="ostrom-implementer-${implement_kill_item_hash:0:16}"
implement_kill_lease="implementer-item-$implement_kill_item_hash.lease"
CLAUDE_CONFIG_DIR="$implement_config" MANDATE_LEASE_NAME="$implement_kill_lease" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$implement_kill_unit" 3600 >/dev/null
implement_kill_marker="$implement_fixture/codex-started"
FAKE_CODEX_MODE=wait FAKE_CODEX_MARKER="$implement_kill_marker" \
  CODEX_BIN="$fake_codex" CLAUDE_CONFIG_DIR="$implement_config" \
  MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
  MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
  FAKE_PR_BODY="$implement_fixture/unused-pr-body" \
  bash "$PLUGIN_ROOT/scripts/implement.sh" \
    "$implement_kill_order" "$implement_kill_unit" \
    >"$implement_fixture/kill.out" 2>"$implement_fixture/kill.err" &
implement_kill_pid=$!
for _attempt in $(seq 1 100); do
  [ -s "$implement_kill_marker" ] && break
  sleep 0.05
done
[ -s "$implement_kill_marker" ]
implement_codex_pid="$(cat "$implement_kill_marker")"
kill -TERM "$implement_kill_pid"
set +e
wait "$implement_kill_pid"
implement_kill_status=$?
set -e
[ "$implement_kill_status" -eq 143 ]
[ ! -e "$implement_config/ostrom/$implement_kill_lease" ]
if kill -0 "$implement_codex_pid" 2>/dev/null; then
  echo "killed implementer left its Codex child running" >&2
  exit 1
fi
jq -s -e '
  length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.item_id == "example-org/example-repo#125"
  and .[0].fact.reason == "signal-TERM"
  and .[0].fact.cost_usd == 0
  and (.[0].fact.duration_seconds | type == "number")
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A clap usage error means the wrapper called Codex incorrectly, not that the
# model failed. Keep that distinct from an ordinary codex-exit-2 terminal row.
implement_usage_candidate="$implement_fixture/usage-candidate.json"
cat >"$implement_usage_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#130","repository":"example-org/example-repo","item_ref":"#130","branch_name":"fix/130-placeholder","spec":"Exercise an invalid harness invocation.","acceptance_criteria":["The usage failure is classified."],"constraints":["Use placeholder data only."]}
JSON
implement_usage_order="$(
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:00:30Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$implement_usage_candidate"
)"
implement_usage_order_id="$(jq -r '.order_id' "$implement_usage_order")"
implement_usage_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#130')"
implement_usage_unit="ostrom-implementer-${implement_usage_item_hash:0:16}"
implement_usage_lease="implementer-item-$implement_usage_item_hash.lease"
CLAUDE_CONFIG_DIR="$implement_config" MANDATE_LEASE_NAME="$implement_usage_lease" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$implement_usage_unit" 3600 >/dev/null
set +e
FAKE_CODEX_MODE=usage \
  CODEX_BIN="$fake_codex" CLAUDE_CONFIG_DIR="$implement_config" \
  MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
  MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
  FAKE_PR_BODY="$implement_fixture/unused-usage-pr-body" \
  bash "$PLUGIN_ROOT/scripts/implement.sh" \
    "$implement_usage_order" "$implement_usage_unit" \
    >"$implement_fixture/usage.out" 2>"$implement_fixture/usage.err"
implement_usage_status=$?
set -e
[ "$implement_usage_status" -eq 2 ]
[ ! -e "$implement_config/ostrom/$implement_usage_lease" ]
jq -s -e --arg order_id "$implement_usage_order_id" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "codex-invocation-invalid"
' "$implement_config/ostrom/sprint.jsonl" >/dev/null
if grep -q '"reason":"codex-exit-2"' "$implement_config/ostrom/sprint.jsonl"; then
  echo "Codex usage error was misclassified as an exec status" >&2
  exit 1
fi

# Codex reports rejected config through exit 1, both as a config.toml load
# failure and as a required-feature-field error. They are harness invocation
# failures too; an unrelated exit-1 remains a model failure.
config_modes=(config config-required model-failure)
config_item_numbers=(131 132 133)
config_expected_reasons=(
  codex-invocation-invalid
  codex-invocation-invalid
  codex-exit-1
)
for config_index in "${!config_modes[@]}"; do
  config_mode="${config_modes[$config_index]}"
  config_item_number="${config_item_numbers[$config_index]}"
  config_expected_reason="${config_expected_reasons[$config_index]}"
  implement_config_candidate="$implement_fixture/$config_mode-candidate.json"
  cat >"$implement_config_candidate" <<JSON
{"schema_version":1,"item_id":"example-org/example-repo#$config_item_number","repository":"example-org/example-repo","item_ref":"#$config_item_number","branch_name":"fix/$config_item_number-placeholder","spec":"Exercise an exit-one harness result.","acceptance_criteria":["The failure is classified."],"constraints":["Use placeholder data only."]}
JSON
  implement_config_order="$(
    CLAUDE_CONFIG_DIR="$implement_config" \
      MANDATE_TRACE_TIME="2026-08-11T03:00:45Z" \
      bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$implement_config_candidate"
  )"
  implement_config_order_id="$(jq -r '.order_id' "$implement_config_order")"
  implement_config_item_id="$(jq -r '.item_id' "$implement_config_order")"
  implement_config_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash "$implement_config_item_id")"
  implement_config_unit="ostrom-implementer-${implement_config_item_hash:0:16}"
  implement_config_lease="implementer-item-$implement_config_item_hash.lease"
  CLAUDE_CONFIG_DIR="$implement_config" MANDATE_LEASE_NAME="$implement_config_lease" \
    bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$implement_config_unit" 3600 >/dev/null
  set +e
  FAKE_CODEX_MODE="$config_mode" \
    CODEX_BIN="$fake_codex" CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
    MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
    FAKE_PR_BODY="$implement_fixture/unused-$config_mode-pr-body" \
    bash "$PLUGIN_ROOT/scripts/implement.sh" \
      "$implement_config_order" "$implement_config_unit" \
      >"$implement_fixture/$config_mode.out" 2>"$implement_fixture/$config_mode.err"
  implement_config_status=$?
  set -e
  [ "$implement_config_status" -eq 1 ]
  [ ! -e "$implement_config/ostrom/$implement_config_lease" ]
  jq -s -e --arg order_id "$implement_config_order_id" \
    --arg reason "$config_expected_reason" '
    [.[] | select(.fact.order_id == $order_id)] | length == 1
    and .[0].kind == "work-failed"
    and .[0].fact.reason == $reason
  ' "$implement_config/ostrom/sprint.jsonl" >/dev/null
done

implement_ok_candidate="$implement_fixture/ok-candidate.json"
cat >"$implement_ok_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#126","repository":"example-org/example-repo","item_ref":"#126","branch_name":"feat/126-placeholder","spec":"Implement a synthetic completed order.","acceptance_criteria":["The generated placeholder file exists."],"constraints":["Use placeholder data only."]}
JSON
implement_ok_order="$(
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:01:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$implement_ok_candidate"
)"
implement_ok_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#126')"
implement_ok_branch="$(jq -r '.branch_name' "$implement_ok_order")"
implement_ok_unit="ostrom-implementer-${implement_ok_item_hash:0:16}"
implement_ok_lease="implementer-item-$implement_ok_item_hash.lease"
CLAUDE_CONFIG_DIR="$implement_config" MANDATE_LEASE_NAME="$implement_ok_lease" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$implement_ok_unit" 3600 >/dev/null
implement_pr_body="$implement_fixture/pr-body"
CODEX_BIN="$fake_codex" CLAUDE_CONFIG_DIR="$implement_config" \
  MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
  MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
  FAKE_PR_BODY="$implement_pr_body" \
  bash "$PLUGIN_ROOT/scripts/implement.sh" \
    "$implement_ok_order" "$implement_ok_unit" \
    >"$implement_fixture/ok.out" 2>"$implement_fixture/ok.err"
grep -q 'https://example.test/pull/125' "$implement_fixture/ok.out"
[ ! -e "$implement_config/ostrom/$implement_ok_lease" ]
git --git-dir="$implement_remote" show-ref --verify \
  "refs/heads/$implement_ok_branch" >/dev/null
[ "$(git --git-dir="$implement_remote" log -1 --format='%(trailers:key=Ostrom-Role,valueonly)' "refs/heads/$implement_ok_branch")" = builder ]
grep -q 'workspace-write' "$implement_pr_body"
grep -q 'approval policy `never`' "$implement_pr_body"
grep -q '^Ostrom-Role: builder$' "$implement_pr_body"
jq -s -e '
  .[-1].kind == "work-completed"
  and .[-1].fact.item_id == "example-org/example-repo#126"
  and .[-1].fact.pr_url == "https://example.test/pull/125"
  and .[-1].fact.reason == null
  and (.[-1].fact.cost_usd | type == "number" and . > 0 and . < 20)
  and .[-1].fact.usage.output_tokens == 50
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# #135: cached input is one tenth the price of fresh input. These observed
# counts are 96.7% cached and weigh 135,356, below the 500,000 ceiling; the
# old single-input formula weighs 894,110 and rejects the completed work.
run_implement_usage_case 134 \
  '{"type":"turn.completed","usage":{"input_tokens":4360176,"cached_input_tokens":4215296,"output_tokens":22074,"reasoning_output_tokens":0}}'
[ "$IMPLEMENT_CASE_STATUS" -eq 0 ]
jq -s -e --arg order_id "$IMPLEMENT_CASE_ORDER_ID" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-completed"
  and .[0].fact.weighted_tokens == 135356
  and .[0].fact.usage.fresh_input_tokens == 144880
  and .[0].fact.usage.cached_input_tokens == 4215296
  and .[0].fact.usage.output_tokens == 22074
  and .[0].fact.usage.cached_input_tokens_available == true
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A completed-turn event that takes cumulative usage over the ceiling stops
# the still-running child. The distinct terminal reason and absence of the
# survival marker prove this is the streaming path, not the post-run backstop.
run_implement_usage_case 138 \
  '{"type":"turn.completed","usage":{"input_tokens":4360176,"cached_input_tokens":0,"output_tokens":22074,"reasoning_output_tokens":0}}' \
  ceiling
[ "$IMPLEMENT_CASE_STATUS" -eq 1 ]
[ -s "$IMPLEMENT_CASE_CODEX_MARKER" ]
if kill -0 "$(cat "$IMPLEMENT_CASE_CODEX_MARKER")" 2>/dev/null; then
  echo "streaming ceiling left its Codex child running" >&2
  exit 1
fi
[ ! -e "$IMPLEMENT_CASE_SURVIVED_MARKER" ]
[ -f "$IMPLEMENT_CASE_WORKTREE/generated.txt" ]
[ -n "$(git -C "$IMPLEMENT_CASE_WORKTREE" status --porcelain)" ]
jq -s -e \
  --arg order_id "$IMPLEMENT_CASE_ORDER_ID" \
  --arg worktree_path "$IMPLEMENT_CASE_WORKTREE" \
  --arg branch_name "$IMPLEMENT_CASE_BRANCH" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "token-ceiling-terminated"
  and .[0].fact.weighted_tokens == 894110
  and .[0].fact.worktree_path == $worktree_path
  and .[0].fact.branch_name == $branch_name
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A large genuinely-fresh run still breaches. The failure row preserves all
# measured components and the dirty worktree so its completed diff is usable.
# Disable streaming supervision here to exercise the retained post-run check.
run_implement_usage_case 135 \
  '{"type":"turn.completed","usage":{"input_tokens":4360176,"cached_input_tokens":0,"output_tokens":22074,"reasoning_output_tokens":0}}' \
  complete disabled
[ "$IMPLEMENT_CASE_STATUS" -eq 1 ]
[ -f "$IMPLEMENT_CASE_WORKTREE/generated.txt" ]
[ -n "$(git -C "$IMPLEMENT_CASE_WORKTREE" status --porcelain)" ]
if git --git-dir="$implement_remote" show-ref --verify \
  "refs/heads/$IMPLEMENT_CASE_BRANCH" >/dev/null 2>&1; then
  echo "over-ceiling implementation was pushed" >&2
  exit 1
fi
if [ -e "$implement_fixture/135-pr-body" ]; then
  echo "over-ceiling implementation opened a pull request" >&2
  exit 1
fi
jq -s -e \
  --arg order_id "$IMPLEMENT_CASE_ORDER_ID" \
  --arg worktree_path "$IMPLEMENT_CASE_WORKTREE" \
  --arg branch_name "$IMPLEMENT_CASE_BRANCH" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "token-ceiling-exceeded"
  and .[0].fact.weighted_tokens == 894110
  and .[0].fact.usage.fresh_input_tokens == 4360176
  and .[0].fact.usage.cached_input_tokens == 0
  and .[0].fact.usage.output_tokens == 22074
  and .[0].fact.usage.cached_input_tokens_available == true
  and .[0].fact.worktree_path == $worktree_path
  and .[0].fact.branch_name == $branch_name
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A harness that omits cached_input_tokens produces an explicit unknown split.
# The ceiling calculation uses the conservative upper bound that all reported
# input was fresh, instead of presenting cached=0 as a measurement.
run_implement_usage_case 136 \
  '{"type":"turn.completed","usage":{"input_tokens":3000000,"output_tokens":0,"reasoning_output_tokens":0}}' \
  complete disabled
[ "$IMPLEMENT_CASE_STATUS" -eq 1 ]
jq -s -e --arg order_id "$IMPLEMENT_CASE_ORDER_ID" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "token-ceiling-exceeded"
  and .[0].fact.weighted_tokens == 600000
  and .[0].fact.usage.fresh_input_tokens == null
  and .[0].fact.usage.cached_input_tokens == null
  and .[0].fact.usage.output_tokens == 0
  and .[0].fact.usage.cached_input_tokens_available == false
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# Worktree preservation applies to every terminal failure after edits, not
# only token-ceiling failures. A model failure leaves a durable path + branch
# pointer and does not stage or commit a possibly incomplete repair.
run_implement_usage_case 139 \
  '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"reasoning_output_tokens":10}}' \
  partial-failure
[ "$IMPLEMENT_CASE_STATUS" -eq 1 ]
[ -f "$IMPLEMENT_CASE_WORKTREE/generated.txt" ]
[ -n "$(git -C "$IMPLEMENT_CASE_WORKTREE" status --porcelain)" ]
jq -s -e \
  --arg order_id "$IMPLEMENT_CASE_ORDER_ID" \
  --arg worktree_path "$IMPLEMENT_CASE_WORKTREE" \
  --arg branch_name "$IMPLEMENT_CASE_BRANCH" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "codex-exit-1"
  and .[0].fact.worktree_path == $worktree_path
  and .[0].fact.branch_name == $branch_name
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A malformed harness report cannot make fresh input or the weight negative.
run_implement_usage_case 137 \
  '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":200,"output_tokens":0,"reasoning_output_tokens":0}}'
[ "$IMPLEMENT_CASE_STATUS" -eq 0 ]
jq -s -e --arg order_id "$IMPLEMENT_CASE_ORDER_ID" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-completed"
  and .[0].fact.weighted_tokens == 4
  and .[0].fact.usage.fresh_input_tokens == 0
  and .[0].fact.usage.cached_input_tokens == 200
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# #123: dispatch must find Codex when it exists only inside nvm, give the
# transient unit the Node interpreter required by Codex's env shebang, and
# leave a matched dispatch/terminal pair after the implementer completes.
nvm_dispatch_home="$implement_fixture/nvm-home"
nvm_dispatch_dir="$nvm_dispatch_home/.nvm"
nvm_old_bin="$nvm_dispatch_dir/versions/node/v24.9.0/bin"
nvm_new_bin="$nvm_dispatch_dir/versions/node/v24.18.0/bin"
mkdir -p "$nvm_dispatch_dir/alias" "$nvm_old_bin" "$nvm_new_bin"
printf '24\n' >"$nvm_dispatch_dir/alias/default"
for fake_nvm_node in "$nvm_old_bin/node" "$nvm_new_bin/node"; do
  cat >"$fake_nvm_node" <<'SH'
#!/usr/bin/env bash
script="$1"
shift
exec bash "$script" "$@"
SH
done
cat >"$nvm_old_bin/codex" <<'SH'
#!/usr/bin/env node
exit 99
SH
cat >"$nvm_new_bin/codex" <<'SH'
#!/usr/bin/env node
if [ "${1:-}" = --version ]; then
  printf 'codex fixture\n'
  exit 0
fi
printf '%s\n' "${1:-}" >>"$FAKE_CODEX_CALLS"
worktree=""
result=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -C) worktree="$2"; shift 2 ;;
    -o) result="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf 'implemented through nvm\n' >"$worktree/generated.txt"
printf 'Synthetic nvm implementation completed.\n' >"$result"
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"reasoning_output_tokens":10}}'
SH
chmod +x "$nvm_old_bin/node" "$nvm_new_bin/node" \
  "$nvm_old_bin/codex" "$nvm_new_bin/codex"

fake_running_systemd="$implement_bin/systemd-run-exec"
cat >"$fake_running_systemd" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_SYSTEMD_ARGS"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --setenv)
      export "$2"
      shift 2
      ;;
    --unit|--description|--property)
      shift 2
      ;;
    --user|--collect|--no-block)
      shift
      ;;
    *)
      exec "$@"
      ;;
  esac
done
exit 2
SH
chmod +x "$fake_running_systemd"

nvm_dispatch_candidate="$implement_fixture/nvm-dispatch-candidate.json"
cat >"$nvm_dispatch_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#127","repository":"example-org/example-repo","item_ref":"#127","branch_name":"feat/127-placeholder","spec":"Implement through an nvm-only Codex fixture.","acceptance_criteria":["The generated placeholder file exists."],"constraints":["Use placeholder data only."]}
JSON
nvm_dispatch_order="$(
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:02:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$nvm_dispatch_candidate"
)"
nvm_dispatch_order_id="$(jq -r '.order_id' "$nvm_dispatch_order")"
nvm_dispatch_args="$implement_fixture/nvm-systemd-args"
nvm_codex_calls="$implement_fixture/nvm-codex-calls"
nvm_dispatch_unit="$(
  env -u CODEX_BIN \
    HOME="$nvm_dispatch_home" NVM_DIR="$nvm_dispatch_dir" \
    PATH="/usr/bin:/bin" OSTROM_NODE_FALLBACKS="" \
    CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:03:00Z" \
    MANDATE_NOW_EPOCH="$cap_today_epoch" \
    MANDATE_GH_AS_BIN="$fake_implement_gh" \
    MANDATE_SYSTEMD_RUN_BIN="$fake_running_systemd" \
    MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
    FAKE_GIT_REMOTE="$implement_remote" \
    FAKE_PR_BODY="$implement_fixture/nvm-pr-body" \
    FAKE_SYSTEMD_ARGS="$nvm_dispatch_args" \
    FAKE_CODEX_CALLS="$nvm_codex_calls" \
    bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$nvm_dispatch_order"
)"
[ -n "$nvm_dispatch_unit" ]
grep -qx 'exec' "$nvm_codex_calls"
grep -qx "PATH=$nvm_new_bin:/usr/bin:/bin" "$nvm_dispatch_args"
jq -s -e --arg order_id "$nvm_dispatch_order_id" '
  [.[] | select(.fact.order_id == $order_id)] as $rows
  | $rows | length == 2
  and .[0].kind == "work-dispatched"
  and .[1].kind == "work-completed"
  and .[0].fact.item_id == .[1].fact.item_id
  and .[0].fact.unit_name == .[1].fact.unit_name
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# A missing Codex fails before systemd launch but still produces a terminal
# protocol row with the stable unavailable reason. It must not masquerade as
# an exec-status failure or create a dangling work-dispatched reservation.
missing_dispatch_config="$implement_fixture/missing-dispatch-config"
mkdir -p "$missing_dispatch_config/ostrom"
missing_dispatch_candidate="$implement_fixture/missing-dispatch-candidate.json"
cat >"$missing_dispatch_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#128","repository":"example-org/example-repo","item_ref":"#128","branch_name":"feat/128-placeholder","spec":"Exercise a missing Codex fixture.","acceptance_criteria":["The failure is classified."],"constraints":["Use placeholder data only."]}
JSON
missing_dispatch_order="$(
  CLAUDE_CONFIG_DIR="$missing_dispatch_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:04:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$missing_dispatch_candidate"
)"
set +e
HOME="$implement_fixture/missing-home" \
  NVM_DIR="$implement_fixture/missing-nvm" \
  PATH="/usr/bin:/bin" OSTROM_NODE_FALLBACKS="" \
  CODEX_BIN="missing-codex" \
  CLAUDE_CONFIG_DIR="$missing_dispatch_config" \
  MANDATE_TRACE_TIME="2026-08-11T03:05:00Z" \
  MANDATE_GH_AS_BIN="$fake_implement_gh" \
  MANDATE_SYSTEMD_RUN_BIN="$fake_running_systemd" \
  FAKE_SYSTEMD_ARGS="$implement_fixture/missing-systemd-args" \
  bash "$PLUGIN_ROOT/scripts/dispatch.sh" "$missing_dispatch_order" \
    >"$implement_fixture/missing.out" 2>"$implement_fixture/missing.err"
missing_dispatch_status=$?
set -e
[ "$missing_dispatch_status" -eq 1 ]
jq -s -e '
  length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "codex-unavailable"
  and .[0].fact.cost_usd == 0
' "$missing_dispatch_config/ostrom/sprint.jsonl" >/dev/null
if grep -q 'codex-exit-' "$missing_dispatch_config/ostrom/sprint.jsonl"; then
  echo "missing Codex was misclassified as an exec status" >&2
  exit 1
fi
[ ! -e "$implement_fixture/missing-systemd-args" ]

# The implementer also preserves that classification if the harness becomes
# unexecutable between dispatch's preflight and the child process launch.
broken_codex="$implement_fixture/broken-codex"
cat >"$broken_codex" <<'SH'
#!/usr/bin/env missing-node-interpreter
SH
chmod +x "$broken_codex"
broken_implement_candidate="$implement_fixture/broken-implement-candidate.json"
cat >"$broken_implement_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#129","repository":"example-org/example-repo","item_ref":"#129","branch_name":"feat/129-placeholder","spec":"Exercise a harness exec failure.","acceptance_criteria":["The failure is classified."],"constraints":["Use placeholder data only."]}
JSON
broken_implement_order="$(
  CLAUDE_CONFIG_DIR="$implement_config" \
    MANDATE_TRACE_TIME="2026-08-11T03:06:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$broken_implement_candidate"
)"
broken_implement_order_id="$(jq -r '.order_id' "$broken_implement_order")"
broken_implement_item_hash="$(bash "$PLUGIN_ROOT/scripts/work-order.sh" item-hash 'example-org/example-repo#129')"
broken_implement_unit="ostrom-implementer-${broken_implement_item_hash:0:16}"
broken_implement_lease="implementer-item-$broken_implement_item_hash.lease"
CLAUDE_CONFIG_DIR="$implement_config" MANDATE_LEASE_NAME="$broken_implement_lease" \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire "$broken_implement_unit" 3600 >/dev/null
set +e
CODEX_BIN="$broken_codex" CLAUDE_CONFIG_DIR="$implement_config" \
  MANDATE_IMPLEMENTER_SOURCE_REPO="$implement_source" \
  MANDATE_GH_AS_BIN="$fake_implement_gh" FAKE_GIT_REMOTE="$implement_remote" \
  FAKE_PR_BODY="$implement_fixture/unused-broken-pr-body" \
  bash "$PLUGIN_ROOT/scripts/implement.sh" \
    "$broken_implement_order" "$broken_implement_unit" \
    >"$implement_fixture/broken.out" 2>"$implement_fixture/broken.err"
broken_implement_status=$?
set -e
[ "$broken_implement_status" -eq 127 ]
jq -s -e --arg order_id "$broken_implement_order_id" '
  [.[] | select(.fact.order_id == $order_id)] | length == 1
  and .[0].kind == "work-failed"
  and .[0].fact.reason == "codex-unavailable"
' "$implement_config/ostrom/sprint.jsonl" >/dev/null

# Triage itself contains no implementation route. This is an effective
# negative assertion, so use an if block rather than `! grep` (#112).
if grep -q 'send a bounded, single-concern change to a subagent' "$work_skill"; then
  echo "builder triage must not implement through an in-process subagent" >&2
  exit 1
fi
grep -q 'scripts/work-order.sh' "$work_skill"
grep -q 'scripts/dispatch.sh' "$work_skill"
grep -Fq 'order_id="$(jq -r '\''.order_id'\'' "$order_file")"' "$work_skill"
grep -q 'filename.*stem is `item_hash`' "$work_skill"
if grep -q 'documented Claude implementer fallback' "$work_skill"; then
  echo "builder protocol names a Claude fallback that does not exist" >&2
  exit 1
fi
grep -q 'order stays undispatched' "$work_skill"

# #134: order_id is the value inside the durable order, never the stable
# filename stem (item_hash). A synthetic builder pass proves the dispatched
# and worked records join one-to-one, while a pre-cutover ambiguous row remains
# structurally readable without being rewritten.
trace_join_config="$fixture/trace-order-join"
trace_join_candidate="$fixture/trace-order-join-candidate.json"
cat >"$trace_join_candidate" <<'JSON'
{"schema_version":1,"item_id":"example-org/example-repo#134","repository":"example-org/example-repo","item_ref":"#134","branch_name":"fix/134-placeholder","spec":"Exercise an unambiguous trace join.","acceptance_criteria":["The trace rows join."],"constraints":["Use placeholder data only."]}
JSON
trace_join_order="$(
  CLAUDE_CONFIG_DIR="$trace_join_config" \
    MANDATE_TRACE_TIME="2026-08-13T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/work-order.sh" create "$trace_join_candidate"
)"
trace_join_order_id="$(jq -r '.order_id' "$trace_join_order")"
trace_join_item_hash="${trace_join_order##*/}"
trace_join_item_hash="${trace_join_item_hash%.json}"
trace_join_unit="ostrom-implementer-${trace_join_item_hash:0:16}"
trace_join_owner="builder-fixture-wake134"
MANDATE_TRACE_TIME="2026-08-13T00:00:01Z" \
  CLAUDE_CONFIG_DIR="$trace_join_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-started \
    "$(jq -cn --arg owner "$trace_join_owner" '{owner:$owner}')" \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-13T00:00:02Z" \
  CLAUDE_CONFIG_DIR="$trace_join_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append work-dispatched \
    "$(jq -cn --arg order_id "$trace_join_order_id" --arg unit_name "$trace_join_unit" \
      '{schema_version:1,item_id:"example-org/example-repo#134",
        order_id:$order_id,unit_name:$unit_name,
        backend:"systemd",cost_ceiling_usd:20,token_ceiling:500000,
        cost_usd:null,duration_seconds:0}')" \
    '{}' >/dev/null
set +e
MANDATE_TRACE_TIME="2026-08-13T00:00:03Z" \
  CLAUDE_CONFIG_DIR="$trace_join_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append item-worked \
    "$(jq -cn --arg owner "$trace_join_owner" \
      --arg order_id "$trace_join_item_hash" \
      '{owner:$owner,repo:"example-org/example-repo",ref:"#134",
        action:"work-order-dispatch",outcome:"completed",order_id:$order_id}')" \
    '{}' >"$fixture/trace-order-join-invalid.out" \
    2>"$fixture/trace-order-join-invalid.err"
trace_join_invalid_status=$?
set -e
[ "$trace_join_invalid_status" -ne 0 ]
grep -q "item-worked order_id.*matches no work order's order_id field" \
  "$fixture/trace-order-join-invalid.err"
[ "$(wc -l <"$trace_join_config/ostrom/sprint.jsonl" | tr -d '[:space:]')" -eq 2 ]
MANDATE_TRACE_TIME="2026-08-13T00:00:04Z" \
  CLAUDE_CONFIG_DIR="$trace_join_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append item-worked \
    "$(jq -cn --arg owner "$trace_join_owner" \
      --arg order_id "$trace_join_order_id" \
      --arg order_file "$trace_join_order" --arg unit_name "$trace_join_unit" \
      '{owner:$owner,repo:"example-org/example-repo",ref:"#134",
        action:"work-order-dispatch",outcome:"completed",order_id:$order_id,
        order_file:$order_file,unit_name:$unit_name}')" \
    '{}' >/dev/null
MANDATE_TRACE_TIME="2026-08-13T00:00:05Z" \
  CLAUDE_CONFIG_DIR="$trace_join_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    "$(jq -cn --arg owner "$trace_join_owner" \
      '{owner:$owner,outcome:"completed",worked_items:1}')" \
    '{}' >/dev/null
jq -s -e '
  map(.kind) == ["pass-started", "work-dispatched", "item-worked", "pass-ended"]
  and ([.[] | select(.kind == "work-dispatched")] | length) == 1
  and ([.[] | select(.kind == "item-worked")] | length) == 1
  and ([.[] | select(.kind == "work-dispatched")] as $dispatched
    | [.[] | select(.kind == "item-worked")] as $worked
    | all($dispatched[];
        . as $dispatch
        | ([$worked[] | select(.fact.order_id == $dispatch.fact.order_id)] | length) == 1))
' "$trace_join_config/ostrom/sprint.jsonl" >/dev/null

historical_trace_config="$fixture/trace-order-historical"
mkdir -p "$historical_trace_config/ostrom"
jq -cn --arg order_id "$trace_join_item_hash" '
  {ts:"2026-08-12T23:59:59Z",kind:"item-worked",
    fact:{owner:"builder-historical-wake1",repo:"example-org/example-repo",
      ref:"#134",action:"work-order-dispatch",outcome:"completed",
      order_id:$order_id},narration:{}}
' >"$historical_trace_config/ostrom/sprint.jsonl"
historical_fact_rows="$(
  CLAUDE_CONFIG_DIR="$historical_trace_config" \
    bash "$PLUGIN_ROOT/scripts/trace.sh" read
)"
jq -s -e --arg order_id "$trace_join_item_hash" '
  length == 1
  and .[0].kind == "item-worked"
  and .[0].fact.order_id == $order_id
  and (.[0] | has("narration") | not)
' <<<"$historical_fact_rows" >/dev/null

# Trace reads make the fact/narration split structural. The ordinary read
# cannot return a top-level narration key; the principal must name the
# narration-specific verb to inspect that region.
trace_config="$fixture/trace"
MANDATE_TRACE_TIME="2026-08-04T00:00:00Z" CLAUDE_CONFIG_DIR="$trace_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append commit \
    '{"sha":"0123456789abcdef"}' \
    '{"reason":"placeholder change"}' >/dev/null
newline_narration='{"reason":"first line\nsecond line with a \"quote\""}'
MANDATE_TRACE_TIME="2026-08-04T00:01:00Z" CLAUDE_CONFIG_DIR="$trace_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append gatekeeper-verdict \
    '{"verdict":"pass","exit_code":0}' \
    "$newline_narration" >/dev/null
trace_file="$trace_config/ostrom/sprint.jsonl"
[ "$(wc -l <"$trace_file" | tr -d '[:space:]')" -eq 2 ]
jq -s -e '
  length == 2
  and .[0] == {
    ts: "2026-08-04T00:00:00Z",
    kind: "commit",
    fact: {sha: "0123456789abcdef"},
    narration: {reason: "placeholder change"}
  }
  and .[1].narration.reason
    == "first line\nsecond line with a \"quote\""
' "$trace_file" >/dev/null
fact_rows="$(
  CLAUDE_CONFIG_DIR="$trace_config" bash "$PLUGIN_ROOT/scripts/trace.sh" read
)"
jq -s -e '
  length == 2
  and all(.[]; has("ts") and has("kind") and has("fact") and (has("narration") | not))
  and .[1].fact == {verdict: "pass", exit_code: 0}
' <<<"$fact_rows" >/dev/null
! grep -q 'narration' <<<"$fact_rows"
narration_rows="$(
  CLAUDE_CONFIG_DIR="$trace_config" \
    bash "$PLUGIN_ROOT/scripts/trace.sh" read-narration
)"
jq -s -e '
  length == 2
  and all(.[]; has("narration") and (has("fact") | not))
  and .[1].narration.reason
    == "first line\nsecond line with a \"quote\""
' <<<"$narration_rows" >/dev/null
oversized_value="$(printf '%*s' 4100 '' | tr ' ' x)"
set +e
CLAUDE_CONFIG_DIR="$trace_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append result \
    "$(jq -cn --arg value "$oversized_value" '{value: $value}')" \
    '{}' >/dev/null 2>&1
oversized_status=$?
set -e
[ "$oversized_status" -eq 2 ]
[ "$(wc -l <"$trace_file" | tr -d '[:space:]')" -eq 2 ]

# Doctor turns missing, stale, current, and expired trace/lease state into one
# deterministic line. It reads MANDATE_NOW_EPOCH rather than the wall clock.
doctor_config="$fixture/doctor"
doctor_absent="$(
  cd "$fixture/repo"
  HOME="$fixture" CLAUDE_CONFIG_DIR="$doctor_config" \
    node "$PLUGIN_ROOT/dist/doctor.js"
)"
grep -q '^WARN|trace-lease|trace absent; lease idle|' <<<"$doctor_absent"
grep -q '^WARN|builder-pass|no builder pass ever recorded|' <<<"$doctor_absent"
grep -q '^WARN|gatekeeper-pass|no gatekeeper pass ever recorded|' <<<"$doctor_absent"

MANDATE_TRACE_TIME="2026-07-30T00:00:00Z" CLAUDE_CONFIG_DIR="$doctor_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake1","outcome":"complete"}' '{}' >/dev/null
doctor_stale="$(
  cd "$fixture/repo"
  HOME="$fixture" CLAUDE_CONFIG_DIR="$doctor_config" \
    node "$PLUGIN_ROOT/dist/doctor.js"
)"
grep -q '^WARN|trace-lease|trace stale, last 2026-07-30T00:00:00Z' \
  <<<"$doctor_stale"
grep -q '^WARN|builder-pass|builder pass stale, last 2026-07-30T00:00:00Z (age 48h; older than 3h cadence)|' \
  <<<"$doctor_stale"
grep -q '^WARN|gatekeeper-pass|no gatekeeper pass ever recorded|' \
  <<<"$doctor_stale"

MANDATE_TRACE_TIME="$MANDATE_SWEEP_TIME" CLAUDE_CONFIG_DIR="$doctor_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"builder-fixture-wake2","outcome":"complete"}' '{}' >/dev/null
MANDATE_TRACE_TIME="$MANDATE_SWEEP_TIME" CLAUDE_CONFIG_DIR="$doctor_config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append pass-ended \
    '{"owner":"gatekeeper-fixture-wake1","outcome":"complete"}' '{}' >/dev/null
doctor_current="$(
  cd "$fixture/repo"
  HOME="$fixture" CLAUDE_CONFIG_DIR="$doctor_config" \
    node "$PLUGIN_ROOT/dist/doctor.js"
)"
grep -q '^OK|trace-lease|trace current, last 2026-08-01T00:00:00Z; lease idle|$' \
  <<<"$doctor_current"
grep -q '^OK|builder-pass|builder pass current, last 2026-08-01T00:00:00Z (age 0m; 3h cadence)|$' \
  <<<"$doctor_current"
grep -q '^OK|gatekeeper-pass|gatekeeper pass current, last 2026-08-01T00:00:00Z (age 0m; 1h cadence)|$' \
  <<<"$doctor_current"

CLAUDE_CONFIG_DIR="$doctor_config" MANDATE_LEASE_NOW_EPOCH=1785538800 \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire gatekeeper-stale 1800 >/dev/null
doctor_expired_lease="$(
  cd "$fixture/repo"
  HOME="$fixture" CLAUDE_CONFIG_DIR="$doctor_config" \
    node "$PLUGIN_ROOT/dist/doctor.js"
)"
grep -q '^WARN|trace-lease|.*lease stale for gatekeeper-stale' \
  <<<"$doctor_expired_lease"

# Prove shipped < user < repo precedence and generic project list parsing.
mkdir -p "$fixture/layers/config/ostrom" "$fixture/layers/repo/.ostrom"
cat >"$fixture/layers/config/ostrom/mandates.yaml" <<'YAML'
cadence_hours: 12
stuck_after_days: 5
search_roots:
  - /placeholder/user-root
bounce_all:
  - label:user-boundary
projects:
  - repo: example-org/example-repo
    delegated:
      - label:user-scope
    excluded:
      - type:docs
    reserved:
      - 17
    default: delegated
    bounce:
      - path:rules/*
YAML
cat >"$fixture/layers/repo/.ostrom/mandates.yaml" <<'YAML'
stuck_after_days: 2
search_roots:
  - /placeholder/repo-root
bounce_all:
  - label:repo-boundary
YAML
layered="$(
  cd "$fixture/layers/repo"
  CLAUDE_CONFIG_DIR="$fixture/layers/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash -c 'source "$CLAUDE_PLUGIN_ROOT/scripts/mandate-lib.sh"; mandate_load_config'
)"
jq -e '
  .provider == "file"
  and .cadence_hours == 12
  and .stuck_after_days == 2
  and .search_roots == ["/placeholder/repo-root"]
  and .bounce_all == ["label:repo-boundary"]
  and .projects[0].repo == "example-org/example-repo"
  and .projects[0].delegated == ["label:user-scope"]
  and .projects[0].excluded == ["type:docs"]
  and .projects[0].reserved == [17]
  and .projects[0].default == "delegated"
  and .projects[0].paused == false
' <<<"$layered" >/dev/null

# A headless Bash tool refuses to statically permit `source "$path"`, since
# sourcing evaluates its argument as shell code. gatekeep/SKILL.md step 3
# works around that by executing mandate-lib.sh directly instead of sourcing
# it (#86's sibling defect). Prove both paths resolve the same layered
# config: sourcing must still define the functions every other script
# relies on, and direct execution must print the identical resolved JSON on
# stdout rather than requiring a second roster parser.
dispatched="$(
  cd "$fixture/layers/repo"
  CLAUDE_CONFIG_DIR="$fixture/layers/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/mandate-lib.sh" config
)"
[ "$dispatched" = "$layered" ]

set +e
dispatch_usage="$(
  bash "$PLUGIN_ROOT/scripts/mandate-lib.sh" 2>&1
)"
dispatch_usage_status=$?
set -e
[ "$dispatch_usage_status" -eq 2 ]
grep -Fq 'usage:' <<<"$dispatch_usage"

assert_bad_selector() {
  name="$1"
  selector="$2"
  expected="$3"
  case_dir="$fixture/lint-$name"
  mkdir -p "$case_dir/config/ostrom" "$case_dir/repo"
  cat >"$case_dir/config/ostrom/mandates.yaml" <<YAML
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated:
      - $selector
    excluded: []
    reserved: []
    bounce: []
YAML
  set +e
  message="$(
    cd "$case_dir/repo"
    CLAUDE_CONFIG_DIR="$case_dir/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash -c 'source "$CLAUDE_PLUGIN_ROOT/scripts/mandate-lib.sh"; mandate_load_config' \
      2>&1
  )"
  status=$?
  set -e
  [ "$status" -eq 2 ]
  grep -Fq "$expected" <<<"$message"
}

# Load-time selector lint is the regression guard for sentence matchers.
assert_bad_selector sentence \
  "platform and pipeline specs — grants and toolchains" \
  "unknown selector prefix"
assert_bad_selector title-star "title:production deployment" \
  "title selector must contain *"
assert_bad_selector title-run "title:*abcdefghijklmnopqrstuvwxyz*" \
  "title selector literal run exceeds 24 characters"

# GitHub App authentication fails closed before any network call. The curl
# stub is the network boundary for every app-token test in this suite.
app_token_fixture="$fixture/app-token"
mkdir -p "$app_token_fixture/bin"
cat >"$app_token_fixture/bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

authorization_config="$(cat)"
[ -n "$authorization_config" ]
url=""
for argument in "$@"; do
  case "$argument" in
    https://api.github.com/*) url="$argument" ;;
  esac
done
if [ -n "${FAKE_CURL_CALL_LOG:-}" ]; then
  printf '%s\n' "$url" >>"$FAKE_CURL_CALL_LOG"
fi

case "${FAKE_CURL_MODE:-transport-failure}:$url" in
  not-installed:https://api.github.com/repos/placeholder-owner/placeholder-repo/installation)
    printf '{"message":"Not Found"}\n404'
    ;;
  success:https://api.github.com/repos/placeholder-owner/placeholder-repo/installation)
    printf '{"id":%s}\n200' "$$"
    ;;
  success:https://api.github.com/app/installations/*/access_tokens)
    printf '{"token":"stub-installation-token"}'
    ;;
  *) exit 99 ;;
esac
EOF
cat >"$app_token_fixture/bin/openssl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  base64)
    base64 | tr -d '\n'
    ;;
  dgst)
    signing_input="$(cat)"
    if [ -n "${FAKE_OPENSSL_EXPECTED_KEY_PATH:-}" ]; then
      [ "${4:-}" = "$FAKE_OPENSSL_EXPECTED_KEY_PATH" ] || exit 98
    fi
    if [ -n "${FAKE_OPENSSL_EXPECTED_APP_ID:-}" ]; then
      encoded_payload="${signing_input#*.}"
      encoded_payload="${encoded_payload%%.*}"
      case $((${#encoded_payload} % 4)) in
        0) payload_padding="" ;;
        2) payload_padding="==" ;;
        3) payload_padding="=" ;;
        *) exit 98 ;;
      esac
      payload_json="$(
        printf '%s' "$encoded_payload$payload_padding" |
          tr '_-' '/+' |
          base64 -d 2>/dev/null
      )"
      actual_app_id="$(jq -er '.iss | tostring' <<<"$payload_json")"
      [ "$actual_app_id" = "$FAKE_OPENSSL_EXPECTED_APP_ID" ] || exit 98
    fi
    printf 'stub-signature'
    ;;
  *) exit 99 ;;
esac
EOF
chmod +x "$app_token_fixture/bin/curl" "$app_token_fixture/bin/openssl"
app_token_curl_log="$app_token_fixture/curl.log"

run_app_token_failure() {
  app_token_name="$1"
  app_token_config_dir="$2"
  shift 2
  app_token_stdout="$app_token_fixture/$app_token_name.stdout"
  app_token_stderr="$app_token_fixture/$app_token_name.stderr"
  rm -f "$app_token_curl_log"
  set +e
  PATH="$app_token_fixture/bin:$PATH" \
    GH_TOKEN="ambient-principal-value" \
    GITHUB_TOKEN="ambient-principal-value" \
    FAKE_CURL_CALL_LOG="$app_token_curl_log" \
    CLAUDE_CONFIG_DIR="$app_token_config_dir" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/app-token.sh" "$@" \
      >"$app_token_stdout" 2>"$app_token_stderr"
  app_token_status=$?
  set -e
  [ "$app_token_status" -eq 2 ]
  [ ! -s "$app_token_stdout" ]
  [ ! -e "$app_token_curl_log" ]
  ! grep -q 'ambient-principal-value' "$app_token_stderr"
}

run_app_token_success() {
  app_token_name="$1"
  app_token_config_dir="$2"
  app_token_role="$3"
  expected_app_id="$4"
  expected_key_path="$5"
  app_token_stdout="$app_token_fixture/$app_token_name.stdout"
  app_token_stderr="$app_token_fixture/$app_token_name.stderr"
  rm -f "$app_token_curl_log"
  PATH="$app_token_fixture/bin:$PATH" \
    GH_TOKEN="ambient-principal-value" \
    GITHUB_TOKEN="ambient-principal-value" \
    FAKE_CURL_MODE=success \
    FAKE_CURL_CALL_LOG="$app_token_curl_log" \
    FAKE_OPENSSL_EXPECTED_APP_ID="$expected_app_id" \
    FAKE_OPENSSL_EXPECTED_KEY_PATH="$expected_key_path" \
    CLAUDE_CONFIG_DIR="$app_token_config_dir" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/app-token.sh" \
      "$app_token_role" placeholder-owner/placeholder-repo \
      >"$app_token_stdout" 2>"$app_token_stderr"
  grep -Fxq 'stub-installation-token' "$app_token_stdout"
  [ ! -s "$app_token_stderr" ]
  [ "$(wc -l <"$app_token_curl_log" | tr -d '[:space:]')" -eq 2 ]
  ! grep -q 'ambient-principal-value' \
    "$app_token_stdout" "$app_token_stderr" "$app_token_curl_log"
}

app_token_missing_argument="$app_token_fixture/missing-argument"
run_app_token_failure missing-argument "$app_token_missing_argument"
grep -Fxq 'app-token: usage: app-token.sh <role> <owner>/<repo>' \
  "$app_token_fixture/missing-argument.stderr"

app_token_invalid_role="$app_token_fixture/invalid-role"
for invalid_role_case in \
  'invalid-empty:' \
  'invalid-slash:gate/keeper' \
  'invalid-space:gate keeper' \
  'invalid-leading-digit:1gatekeeper'; do
  invalid_role_name="${invalid_role_case%%:*}"
  invalid_role="${invalid_role_case#*:}"
  run_app_token_failure "$invalid_role_name" "$app_token_invalid_role" \
    "$invalid_role" placeholder-owner/placeholder-repo
  grep -Fxq \
    'app-token: invalid role: must match [a-z][a-z0-9_-]*' \
    "$app_token_fixture/$invalid_role_name.stderr"
done

app_token_missing_secrets="$app_token_fixture/missing-secrets"
run_app_token_failure missing-secrets "$app_token_missing_secrets" \
  gatekeeper placeholder-owner/placeholder-repo
grep -Fxq \
  'app-token: secrets file is missing at the configured Ostrom secrets path' \
  "$app_token_fixture/missing-secrets.stderr"

app_token_missing_role_block="$app_token_fixture/missing-role-block"
mkdir -p "$app_token_missing_role_block/ostrom"
cat >"$app_token_missing_role_block/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: 1 # APP_ID_PLACEHOLDER
  private_key_path: $app_token_missing_role_block/gatekeeper-placeholder.pem
YAML
run_app_token_failure missing-role-block "$app_token_missing_role_block" \
  builder placeholder-owner/placeholder-repo
grep -Fxq 'app-token: neither builder nor shared credentials are configured' \
  "$app_token_fixture/missing-role-block.stderr"

app_token_builder="$app_token_fixture/builder"
mkdir -p "$app_token_builder/ostrom"
app_token_builder_key="$app_token_builder/builder-placeholder.pem"
: >"$app_token_builder_key"
cat >"$app_token_builder/ostrom/secrets.yaml" <<YAML
builder:
  app_id: 2 # APP_ID_PLACEHOLDER
  private_key_path: $app_token_builder_key
YAML
run_app_token_success builder "$app_token_builder" builder \
  2 "$app_token_builder_key"

# A role with no dedicated block resolves to the shared App. The signer stub
# checks both fields, so this proves fallback selected the shared credential
# rather than merely reaching the network with some parseable values.
app_token_shared="$app_token_fixture/shared"
mkdir -p "$app_token_shared/ostrom"
app_token_shared_key="$app_token_shared/shared-placeholder.pem"
: >"$app_token_shared_key"
cat >"$app_token_shared/ostrom/secrets.yaml" <<YAML
shared:
  app_id: 3 # APP_ID_PLACEHOLDER_SHARED
  private_key_path: $app_token_shared_key
YAML
run_app_token_success shared-fallback "$app_token_shared" reporter \
  3 "$app_token_shared_key"

# An explicit role block remains the reversible override during cutover. A
# different shared ID and key make accidental fallback observable here.
app_token_role_override="$app_token_fixture/role-override"
mkdir -p "$app_token_role_override/ostrom"
app_token_role_override_key="$app_token_role_override/builder-placeholder.pem"
app_token_role_override_shared_key="$app_token_role_override/shared-placeholder.pem"
: >"$app_token_role_override_key"
: >"$app_token_role_override_shared_key"
cat >"$app_token_role_override/ostrom/secrets.yaml" <<YAML
shared:
  app_id: 3 # APP_ID_PLACEHOLDER_SHARED
  private_key_path: $app_token_role_override_shared_key
builder:
  app_id: 4 # APP_ID_PLACEHOLDER_BUILDER
  private_key_path: $app_token_role_override_key
YAML
run_app_token_success role-override "$app_token_role_override" builder \
  4 "$app_token_role_override_key"

# A malformed role override must not silently fall through to a valid shared
# block. Presence selects the override; validity decides whether minting may
# continue.
app_token_malformed_override="$app_token_fixture/malformed-override"
mkdir -p "$app_token_malformed_override/ostrom"
cat >"$app_token_malformed_override/ostrom/secrets.yaml" <<YAML
shared:
  app_id: 3 # APP_ID_PLACEHOLDER_SHARED
  private_key_path: $app_token_shared_key
builder:
  app_id: 4 # APP_ID_PLACEHOLDER_BUILDER
YAML
run_app_token_failure malformed-override "$app_token_malformed_override" \
  builder placeholder-owner/placeholder-repo
grep -Fxq 'app-token: missing required builder field: private_key_path' \
  "$app_token_fixture/malformed-override.stderr"

# A present but malformed shared block is an authentication failure, never a
# reason to try ambient principal credentials.
app_token_malformed_shared="$app_token_fixture/malformed-shared"
mkdir -p "$app_token_malformed_shared/ostrom"
cat >"$app_token_malformed_shared/ostrom/secrets.yaml" <<YAML
shared:
  app_id: APP_ID_PLACEHOLDER_INVALID
  private_key_path: $app_token_shared_key
YAML
run_app_token_failure malformed-shared "$app_token_malformed_shared" \
  gatekeeper placeholder-owner/placeholder-repo
grep -Fxq 'app-token: shared app_id must be a positive integer' \
  "$app_token_fixture/malformed-shared.stderr"

# With both role blocks configured, the signer sees the app ID and key path
# belonging to the requested role. The stub rejects either cross-role mix-up.
app_token_both_roles="$app_token_fixture/both-roles"
mkdir -p "$app_token_both_roles/ostrom"
app_token_both_gatekeeper_key="$app_token_both_roles/gatekeeper-placeholder.pem"
app_token_both_builder_key="$app_token_both_roles/builder-placeholder.pem"
: >"$app_token_both_gatekeeper_key"
: >"$app_token_both_builder_key"
cat >"$app_token_both_roles/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: 1 # APP_ID_PLACEHOLDER_GATEKEEPER
  private_key_path: $app_token_both_gatekeeper_key
builder:
  app_id: 2 # APP_ID_PLACEHOLDER_BUILDER
  private_key_path: $app_token_both_builder_key
YAML
run_app_token_success both-roles-gatekeeper "$app_token_both_roles" gatekeeper \
  1 "$app_token_both_gatekeeper_key"
run_app_token_success both-roles-builder "$app_token_both_roles" builder \
  2 "$app_token_both_builder_key"

app_token_missing_key="$app_token_fixture/missing-key"
mkdir -p "$app_token_missing_key/ostrom"
cat >"$app_token_missing_key/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: $$
  private_key_path: $app_token_missing_key/absent.pem
YAML
run_app_token_failure missing-key "$app_token_missing_key" \
  gatekeeper placeholder-owner/placeholder-repo
grep -Fxq 'app-token: gatekeeper private key file is missing or unreadable' \
  "$app_token_fixture/missing-key.stderr"

app_token_missing_field="$app_token_fixture/missing-field"
mkdir -p "$app_token_missing_field/ostrom"
cat >"$app_token_missing_field/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: $$
YAML
run_app_token_failure missing-field "$app_token_missing_field" \
  gatekeeper placeholder-owner/placeholder-repo
grep -Fxq 'app-token: missing required gatekeeper field: private_key_path' \
  "$app_token_fixture/missing-field.stderr"

# A repository lookup 404 names the repository and stops before the token
# exchange. Ambient principal credentials are neither returned nor used.
app_token_not_installed="$app_token_fixture/not-installed"
mkdir -p "$app_token_not_installed/ostrom"
app_token_not_installed_key="$app_token_not_installed/placeholder.pem"
: >"$app_token_not_installed_key"
cat >"$app_token_not_installed/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: $$
  private_key_path: $app_token_not_installed_key
YAML
rm -f "$app_token_curl_log"
set +e
PATH="$app_token_fixture/bin:$PATH" \
  GH_TOKEN="ambient-principal-value" \
  GITHUB_TOKEN="ambient-principal-value" \
  FAKE_CURL_MODE=not-installed \
  FAKE_CURL_CALL_LOG="$app_token_curl_log" \
  CLAUDE_CONFIG_DIR="$app_token_not_installed" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/app-token.sh" \
    gatekeeper placeholder-owner/placeholder-repo \
    >"$app_token_fixture/not-installed.stdout" \
    2>"$app_token_fixture/not-installed.stderr"
app_token_not_installed_status=$?
set -e
[ "$app_token_not_installed_status" -eq 2 ]
[ ! -s "$app_token_fixture/not-installed.stdout" ]
grep -Fxq \
  'app-token: GitHub App is not installed on repository placeholder-owner/placeholder-repo' \
  "$app_token_fixture/not-installed.stderr"
! grep -q 'ambient-principal-value' "$app_token_fixture/not-installed.stderr"
[ "$(wc -l <"$app_token_curl_log" | tr -d '[:space:]')" -eq 1 ]
grep -Fxq \
  'https://api.github.com/repos/placeholder-owner/placeholder-repo/installation' \
  "$app_token_curl_log"
! grep -q '/access_tokens' "$app_token_curl_log"

# A stale installation_id is accepted but discarded; the repository lookup,
# not that obsolete value, selects the installation used for the exchange.
app_token_stale_id="$app_token_fixture/stale-id"
mkdir -p "$app_token_stale_id/ostrom"
app_token_stale_key="$app_token_stale_id/placeholder.pem"
: >"$app_token_stale_key"
cat >"$app_token_stale_id/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: $$
  installation_id: <OBSOLETE_INSTALLATION_ID>
  private_key_path: $app_token_stale_key
YAML
rm -f "$app_token_curl_log"
PATH="$app_token_fixture/bin:$PATH" \
  GH_TOKEN="ambient-principal-value" \
  GITHUB_TOKEN="ambient-principal-value" \
  FAKE_CURL_MODE=success \
  FAKE_CURL_CALL_LOG="$app_token_curl_log" \
  CLAUDE_CONFIG_DIR="$app_token_stale_id" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/app-token.sh" \
    gatekeeper placeholder-owner/placeholder-repo \
    >"$app_token_fixture/stale-id.stdout" \
    2>"$app_token_fixture/stale-id.stderr"
grep -Fxq 'stub-installation-token' "$app_token_fixture/stale-id.stdout"
[ ! -s "$app_token_fixture/stale-id.stderr" ]
[ "$(wc -l <"$app_token_curl_log" | tr -d '[:space:]')" -eq 2 ]
grep -Fxq \
  'https://api.github.com/repos/placeholder-owner/placeholder-repo/installation' \
  "$app_token_curl_log"
grep -Eq \
  '^https://api.github.com/app/installations/[1-9][0-9]*/access_tokens$' \
  "$app_token_curl_log"
! grep -q 'OBSOLETE_INSTALLATION_ID' \
  "$app_token_fixture/stale-id.stdout" \
  "$app_token_fixture/stale-id.stderr" \
  "$app_token_curl_log"

# gh-as.sh is the only sanctioned way a session-issued command mints and
# uses a role token: a session's Bash tool cannot itself capture
# app-token.sh's stdout into a variable, so gh-as.sh does that internally and
# execs the given command with the token exported only in its own process.
# These tests are the regression guard for #93's central property: the token
# must never appear in anything gh-as.sh itself writes, while still actually
# reaching the wrapped command's environment.
gh_as_fixture="$fixture/gh-as"
mkdir -p "$gh_as_fixture/bin" "$gh_as_fixture/config/ostrom"
gh_as_key="$gh_as_fixture/gatekeeper-placeholder.pem"
: >"$gh_as_key"
cat >"$gh_as_fixture/config/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: $$
  private_key_path: $gh_as_key
YAML

# A fake gh that behaves like the real one: it never echoes GH_TOKEN or
# GITHUB_TOKEN to its own stdout or stderr. It records what it actually
# received on a side channel instead, so the test can confirm the token was
# delivered without trusting output a real `gh` would never produce either.
# `exit-code <n>` lets a test control the wrapped command's own exit status,
# to prove gh-as.sh passes it through unchanged.
cat >"$gh_as_fixture/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
: >"$GH_AS_TEST_SEEN_FILE"
printf '%s\n' "${GH_TOKEN:-}" >>"$GH_AS_TEST_SEEN_FILE"
printf '%s\n' "${GITHUB_TOKEN:-}" >>"$GH_AS_TEST_SEEN_FILE"
printf 'gh-as-test: called with %s\n' "$*"
if [ "${1:-}" = "exit-code" ]; then
  exit "$2"
fi
EOF
chmod +x "$gh_as_fixture/bin/gh"

# Success: the token reaches the wrapped command's environment, but never
# gh-as.sh's own stdout or stderr, and its exit code is the wrapped
# command's own, unchanged.
gh_as_seen="$gh_as_fixture/token-seen"
gh_as_stdout="$gh_as_fixture/success.stdout"
gh_as_stderr="$gh_as_fixture/success.stderr"
rm -f "$gh_as_seen" "$app_token_curl_log"
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  FAKE_CURL_CALL_LOG="$app_token_curl_log" \
  GH_AS_TEST_SEEN_FILE="$gh_as_seen" \
  CLAUDE_CONFIG_DIR="$gh_as_fixture/config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" gatekeeper \
    placeholder-owner/placeholder-repo \
    gh pr list --repo placeholder-owner/placeholder-repo \
    >"$gh_as_stdout" 2>"$gh_as_stderr"
gh_as_status=$?
[ "$gh_as_status" -eq 0 ]
grep -Fxq 'stub-installation-token' <(sed -n '1p' "$gh_as_seen")
grep -Fxq 'stub-installation-token' <(sed -n '2p' "$gh_as_seen")
grep -Fq 'gh-as-test: called with pr list --repo placeholder-owner/placeholder-repo' \
  "$gh_as_stdout"
[ ! -s "$gh_as_stderr" ]
! grep -q 'stub-installation-token' "$gh_as_stdout" "$gh_as_stderr"

# gh-as.sh's own exit-111 space and the wrapped command's exit codes stay
# distinguishable: a wrapped command that itself fails passes its own code
# through unchanged, never 111.
gh_as_wrapped_fail_stdout="$gh_as_fixture/wrapped-fail.stdout"
gh_as_wrapped_fail_stderr="$gh_as_fixture/wrapped-fail.stderr"
rm -f "$gh_as_seen"
set +e
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  GH_AS_TEST_SEEN_FILE="$gh_as_seen" \
  CLAUDE_CONFIG_DIR="$gh_as_fixture/config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" gatekeeper \
    placeholder-owner/placeholder-repo \
    gh exit-code 7 \
    >"$gh_as_wrapped_fail_stdout" 2>"$gh_as_wrapped_fail_stderr"
gh_as_wrapped_fail_status=$?
set -e
[ "$gh_as_wrapped_fail_status" -eq 7 ]
[ -s "$gh_as_seen" ]
! grep -q 'stub-installation-token' \
  "$gh_as_wrapped_fail_stdout" "$gh_as_wrapped_fail_stderr"

# Failure: minting fails closed (no secrets configured), the wrapped command
# never runs at all, and an ambient credential already present in the
# session's own environment is neither used nor leaked into the failure
# output. Exit 111 is reserved for exactly this path.
gh_as_no_secrets="$gh_as_fixture/no-secrets"
mkdir -p "$gh_as_no_secrets/ostrom"
gh_as_auth_fail_stdout="$gh_as_fixture/auth-fail.stdout"
gh_as_auth_fail_stderr="$gh_as_fixture/auth-fail.stderr"
rm -f "$gh_as_seen"
set +e
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="ambient-principal-value" \
  GITHUB_TOKEN="ambient-principal-value" \
  GH_AS_TEST_SEEN_FILE="$gh_as_seen" \
  CLAUDE_CONFIG_DIR="$gh_as_no_secrets" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" gatekeeper \
    placeholder-owner/placeholder-repo \
    gh pr list --repo placeholder-owner/placeholder-repo \
    >"$gh_as_auth_fail_stdout" 2>"$gh_as_auth_fail_stderr"
gh_as_auth_fail_status=$?
set -e
[ "$gh_as_auth_fail_status" -eq 111 ]
[ ! -s "$gh_as_auth_fail_stdout" ]
[ ! -e "$gh_as_seen" ]
grep -Fq 'gh-as: could not mint a gatekeeper token for placeholder-owner/placeholder-repo' \
  "$gh_as_auth_fail_stderr"
! grep -q 'ambient-principal-value' \
  "$gh_as_auth_fail_stdout" "$gh_as_auth_fail_stderr"
! grep -q 'stub-installation-token' \
  "$gh_as_auth_fail_stdout" "$gh_as_auth_fail_stderr"

# Role resolution holds through the wrapper: a builder call against a config
# with neither builder nor shared credentials fails rather than silently
# reaching the gatekeeper's compatibility block.
gh_as_role_stdout="$gh_as_fixture/wrong-role.stdout"
gh_as_role_stderr="$gh_as_fixture/wrong-role.stderr"
set +e
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  CLAUDE_CONFIG_DIR="$gh_as_fixture/config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" builder \
    placeholder-owner/placeholder-repo \
    gh pr list --repo placeholder-owner/placeholder-repo \
    >"$gh_as_role_stdout" 2>"$gh_as_role_stderr"
gh_as_role_status=$?
set -e
[ "$gh_as_role_status" -eq 111 ]
[ ! -s "$gh_as_role_stdout" ]
grep -Fq 'neither builder nor shared credentials are configured' \
  "$gh_as_role_stderr"

# `git` does not read GH_TOKEN itself, so gh-as.sh also points it at a
# credential helper -- `gh auth git-credential`, which does -- scoped to
# this one process via the GIT_CONFIG_COUNT/KEY/VALUE environment form,
# never via `git config` or `gh auth setup-git`, either of which would
# leave a helper reference behind in a config file on disk after this
# process exits. This is #41's regression guard: the credential helper must
# reach a wrapped `git` the same way GH_TOKEN reaches a wrapped `gh`, must
# clear any credential.helper already configured elsewhere first (an
# operator's own helper trying first could otherwise satisfy the request
# with the operator's own stored credentials, silently defeating the whole
# point), and must never appear in gh-as.sh's own stdout or stderr.
# gh-as.sh probes `git --version` before doing anything else, so every fake
# `git` in this file has to answer that call the way a real, current git
# would; this one reports a version comfortably above 2.31 so the version
# check falls through to the normal path exercised below.
cat >"$gh_as_fixture/bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--version" ]; then
  printf 'git version 2.43.0\n'
  exit 0
fi
: >"$GH_AS_GIT_SEEN_FILE"
{
  printf 'count=%s\n' "${GIT_CONFIG_COUNT:-}"
  printf 'key0=%s\n' "${GIT_CONFIG_KEY_0:-}"
  printf 'value0=%s\n' "${GIT_CONFIG_VALUE_0:-}"
  printf 'key1=%s\n' "${GIT_CONFIG_KEY_1:-}"
  printf 'value1=%s\n' "${GIT_CONFIG_VALUE_1:-}"
  printf 'gh_token=%s\n' "${GH_TOKEN:-}"
} >>"$GH_AS_GIT_SEEN_FILE"
printf 'gh-as-git-test: called with %s\n' "$*"
EOF
chmod +x "$gh_as_fixture/bin/git"

gh_as_git_seen="$gh_as_fixture/git-token-seen"
gh_as_git_stdout="$gh_as_fixture/git-push.stdout"
gh_as_git_stderr="$gh_as_fixture/git-push.stderr"
rm -f "$gh_as_git_seen"
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  CLAUDE_CONFIG_DIR="$app_token_builder" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  GH_AS_GIT_SEEN_FILE="$gh_as_git_seen" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" builder \
    placeholder-owner/placeholder-repo \
    git push https://github.com/placeholder-owner/placeholder-repo.git \
    HEAD:refs/heads/placeholder \
    >"$gh_as_git_stdout" 2>"$gh_as_git_stderr"
gh_as_git_status=$?
[ "$gh_as_git_status" -eq 0 ]
[ -s "$gh_as_git_seen" ]
grep -Fxq 'count=2' "$gh_as_git_seen"
grep -Fxq 'key0=credential.helper' "$gh_as_git_seen"
grep -Fxq 'value0=' "$gh_as_git_seen"
grep -Fxq 'key1=credential.helper' "$gh_as_git_seen"
grep -Fxq 'value1=!gh auth git-credential' "$gh_as_git_seen"
grep -Fxq 'gh_token=stub-installation-token' "$gh_as_git_seen"
grep -Fq 'gh-as-git-test: called with push https://github.com/placeholder-owner/placeholder-repo.git HEAD:refs/heads/placeholder' \
  "$gh_as_git_stdout"
[ ! -s "$gh_as_git_stderr" ]
! grep -q 'stub-installation-token' "$gh_as_git_stdout" "$gh_as_git_stderr"

# #97 review: the GIT_CONFIG_COUNT/KEY/VALUE form is only honoured from Git
# 2.31 onward, and an older git ignores it rather than erroring, so a stale
# git would silently fall back to whatever credential.helper the operator
# already has configured -- exactly the leak this script exists to close.
# gh-as.sh checks the wrapped git's version before minting anything, and
# only when the wrapped command is `git` itself.
gh_as_old_git_bin="$gh_as_fixture/bin-old-git"
mkdir -p "$gh_as_old_git_bin"
cat >"$gh_as_old_git_bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--version" ]; then
  printf 'git version 2.30.2\n'
  exit 0
fi
# gh-as.sh should never reach here for an old git -- if it does, the
# version check did not fail closed, and the test must catch that.
: >"$GH_AS_GIT_SEEN_FILE"
printf 'gh-as-old-git-test: called with %s\n' "$*" >>"$GH_AS_GIT_SEEN_FILE"
EOF
chmod +x "$gh_as_old_git_bin/git"

gh_as_old_git_seen="$gh_as_fixture/old-git-seen"
gh_as_old_git_stdout="$gh_as_fixture/old-git.stdout"
gh_as_old_git_stderr="$gh_as_fixture/old-git.stderr"
rm -f "$gh_as_old_git_seen" "$app_token_curl_log"
set +e
PATH="$gh_as_old_git_bin:$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  FAKE_CURL_CALL_LOG="$app_token_curl_log" \
  CLAUDE_CONFIG_DIR="$app_token_builder" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  GH_AS_GIT_SEEN_FILE="$gh_as_old_git_seen" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" builder \
    placeholder-owner/placeholder-repo \
    git push https://github.com/placeholder-owner/placeholder-repo.git \
    HEAD:refs/heads/placeholder \
    >"$gh_as_old_git_stdout" 2>"$gh_as_old_git_stderr"
gh_as_old_git_status=$?
set -e
[ "$gh_as_old_git_status" -eq 111 ]
[ ! -e "$gh_as_old_git_seen" ]
[ ! -e "$app_token_curl_log" ]
[ ! -s "$gh_as_old_git_stdout" ]
grep -Fq '2.30.2' "$gh_as_old_git_stderr"
grep -Fq 'older than 2.31' "$gh_as_old_git_stderr"

# The mirror image: a git new enough to honour GIT_CONFIG_COUNT/KEY/VALUE
# clears the check and reaches the same normal path proven above.
gh_as_new_git_bin="$gh_as_fixture/bin-new-git"
mkdir -p "$gh_as_new_git_bin"
cat >"$gh_as_new_git_bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--version" ]; then
  printf 'git version 2.45.0\n'
  exit 0
fi
: >"$GH_AS_GIT_SEEN_FILE"
{
  printf 'count=%s\n' "${GIT_CONFIG_COUNT:-}"
  printf 'key0=%s\n' "${GIT_CONFIG_KEY_0:-}"
  printf 'value0=%s\n' "${GIT_CONFIG_VALUE_0:-}"
  printf 'key1=%s\n' "${GIT_CONFIG_KEY_1:-}"
  printf 'value1=%s\n' "${GIT_CONFIG_VALUE_1:-}"
  printf 'gh_token=%s\n' "${GH_TOKEN:-}"
} >>"$GH_AS_GIT_SEEN_FILE"
printf 'gh-as-new-git-test: called with %s\n' "$*"
EOF
chmod +x "$gh_as_new_git_bin/git"

gh_as_new_git_seen="$gh_as_fixture/new-git-seen"
gh_as_new_git_stdout="$gh_as_fixture/new-git.stdout"
gh_as_new_git_stderr="$gh_as_fixture/new-git.stderr"
rm -f "$gh_as_new_git_seen"
PATH="$gh_as_new_git_bin:$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  CLAUDE_CONFIG_DIR="$app_token_builder" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  GH_AS_GIT_SEEN_FILE="$gh_as_new_git_seen" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" builder \
    placeholder-owner/placeholder-repo \
    git push https://github.com/placeholder-owner/placeholder-repo.git \
    HEAD:refs/heads/placeholder \
    >"$gh_as_new_git_stdout" 2>"$gh_as_new_git_stderr"
gh_as_new_git_status=$?
[ "$gh_as_new_git_status" -eq 0 ]
[ -s "$gh_as_new_git_seen" ]
grep -Fxq 'count=2' "$gh_as_new_git_seen"
grep -Fxq 'key0=credential.helper' "$gh_as_new_git_seen"
grep -Fxq 'value0=' "$gh_as_new_git_seen"
grep -Fxq 'key1=credential.helper' "$gh_as_new_git_seen"
grep -Fxq 'value1=!gh auth git-credential' "$gh_as_new_git_seen"
grep -Fxq 'gh_token=stub-installation-token' "$gh_as_new_git_seen"
grep -Fq 'gh-as-new-git-test: called with push https://github.com/placeholder-owner/placeholder-repo.git HEAD:refs/heads/placeholder' \
  "$gh_as_new_git_stdout"
[ ! -s "$gh_as_new_git_stderr" ]

# The check keys off the wrapped command being `git` itself, not off
# whether the system happens to have an old git lying around: a wrapped
# `gh` call must succeed normally even with the old fake `git` from above
# still first on PATH.
gh_as_nongit_seen="$gh_as_fixture/nongit-seen"
gh_as_nongit_stdout="$gh_as_fixture/nongit.stdout"
gh_as_nongit_stderr="$gh_as_fixture/nongit.stderr"
rm -f "$gh_as_nongit_seen"
PATH="$gh_as_old_git_bin:$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  GH_AS_TEST_SEEN_FILE="$gh_as_nongit_seen" \
  CLAUDE_CONFIG_DIR="$gh_as_fixture/config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" gatekeeper \
    placeholder-owner/placeholder-repo \
    gh pr list --repo placeholder-owner/placeholder-repo \
    >"$gh_as_nongit_stdout" 2>"$gh_as_nongit_stderr"
gh_as_nongit_status=$?
[ "$gh_as_nongit_status" -eq 0 ]
grep -Fxq 'stub-installation-token' <(sed -n '1p' "$gh_as_nongit_seen")
grep -Fq 'gh-as-test: called with pr list --repo placeholder-owner/placeholder-repo' \
  "$gh_as_nongit_stdout"
[ ! -s "$gh_as_nongit_stderr" ]

# The `*/git` arm of the pattern exists so a wrapped command given as a
# path is recognised too -- but the version probe has to run that exact
# path, not a bare `git`, or a modern git elsewhere on PATH would mask a
# stale one at the path actually being exec'd. PATH here resolves bare
# `git` to the modern fixture above; only the explicit path points at the
# old one. If the check ever regressed to probing `git --version` instead
# of "$1" --version, this would wrongly pass.
gh_as_old_git_by_path_seen="$gh_as_fixture/old-git-by-path-seen"
gh_as_old_git_by_path_stdout="$gh_as_fixture/old-git-by-path.stdout"
gh_as_old_git_by_path_stderr="$gh_as_fixture/old-git-by-path.stderr"
rm -f "$gh_as_old_git_by_path_seen" "$app_token_curl_log"
set +e
PATH="$gh_as_fixture/bin:$app_token_fixture/bin:$PATH" \
  GH_TOKEN="" \
  GITHUB_TOKEN="" \
  FAKE_CURL_MODE=success \
  FAKE_CURL_CALL_LOG="$app_token_curl_log" \
  CLAUDE_CONFIG_DIR="$app_token_builder" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  GH_AS_GIT_SEEN_FILE="$gh_as_old_git_by_path_seen" \
  bash "$PLUGIN_ROOT/scripts/gh-as.sh" builder \
    placeholder-owner/placeholder-repo \
    "$gh_as_old_git_bin/git" push \
    https://github.com/placeholder-owner/placeholder-repo.git \
    HEAD:refs/heads/placeholder \
    >"$gh_as_old_git_by_path_stdout" 2>"$gh_as_old_git_by_path_stderr"
gh_as_old_git_by_path_status=$?
set -e
[ "$gh_as_old_git_by_path_status" -eq 111 ]
[ ! -e "$gh_as_old_git_by_path_seen" ]
[ ! -e "$app_token_curl_log" ]
[ ! -s "$gh_as_old_git_by_path_stdout" ]
grep -Fq '2.30.2' "$gh_as_old_git_by_path_stderr"
grep -Fq 'older than 2.31' "$gh_as_old_git_by_path_stderr"

cat >"$fixture/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

repo="-"
previous=""
for argument in "$@"; do
  if [ "$previous" = "--repo" ]; then
    repo="$argument"
    break
  fi
  previous="$argument"
done
if [ -n "${FAKE_GH_CALL_LOG:-}" ]; then
  printf '%s\t%s\n' "$repo" "$*" >>"$FAKE_GH_CALL_LOG"
fi

# #109: refuse a call whose GH_TOKEN doesn't match the organisation being
# queried, the same way the real GitHub API 404s a token minted for one
# installation when it's used against a repository under another. The curl
# fake mints "stub-sweep-token-org-<id>" where <id> is derived from the
# repository's own owner, so this is only ever a no-op for a correctly
# org-scoped token -- it exists to fail loudly if sweep.sh ever goes back
# to minting one token for a roster that spans more than one organisation.
case "$repo" in
  */*)
    owner="${repo%%/*}"
    expected_id="$(printf '%s' "$owner" | cksum | awk '{print $1}')"
    expected_token="stub-sweep-token-org-$expected_id"
    if [ -n "${GH_TOKEN:-}" ] && [ "${GH_TOKEN:-}" != "$expected_token" ]; then
      echo '{"message":"Not Found","status":"404"}' >&2
      exit 1
    fi
    ;;
esac

if [ "$1 $2" = "auth status" ]; then
  [ "${FAKE_GH_AUTH_FAIL:-0}" != "1" ]
  exit 0
fi
if [ "$1 $2" = "issue list" ]; then
  case "$repo" in
    example-org/example-repo)
      issue7_title="feat(tooling): improve runner"
      issue7_body=""
      if [ "${FAKE_GH_MODE:-base}" = "changed" ]; then
        issue7_title="feat(tooling): improve runner title refreshed upstream"
      fi
      if [ "${FAKE_GH_MODE:-base}" = "dependency-changed" ]; then
        issue7_body="Depends on #168."
      fi
      cat <<JSON
[{"number":7,"title":"$issue7_title","body":"$issue7_body","labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/7"},{"number":9,"title":"Untriaged request","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/9"},{"number":10,"title":"feat(tooling): owner gate","body":"Part of #167. Depends on #168.","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/10"},{"number":11,"title":"Path-only issue","labels":[],"files":[{"path":"docs/guide.md"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/11"},{"number":14,"title":"Rotate credential safely","body":"Part of #167","labels":[{"name":"ignored"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/14"},{"number":15,"title":"Routine excluded work","labels":[{"name":"ignored"},{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/15"}]
JSON
      ;;
    example-org/another-repo)
      cat <<'JSON'
[{"number":20,"title":"Prepare production deployment","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/20"}]
JSON
      ;;
    example-org/policy-cursor-repo)
      policy_cursor_title="Routine delegated work"
      policy_cursor_updated="2026-07-30T00:00:00Z"
      if [ "${FAKE_GH_MODE:-base}" = "policy-pending" ]; then
        policy_cursor_title="Routine delegated work, pending"
        policy_cursor_updated="2026-08-02T00:00:00Z"
      elif [ "${FAKE_GH_MODE:-base}" = "policy-changed" ]; then
        policy_cursor_title="Routine delegated work, changed with policy"
        policy_cursor_updated="2026-08-03T00:00:00Z"
      fi
      cat <<JSON
[{"number":1,"title":"$policy_cursor_title","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"$policy_cursor_updated","url":"https://example.invalid/issues/1"}]
JSON
      ;;
    example-org/incremental-repo)
      incremental_one_title="Routine issue one"
      incremental_one_updated="2026-07-30T00:00:00Z"
      incremental_two_title="Routine issue two"
      incremental_two_updated="2026-07-30T00:00:00Z"
      case "${FAKE_GH_MODE:-base}" in
        incremental-delta)
          incremental_one_title="Routine issue one updated after cursor"
          incremental_one_updated="2026-08-01T00:30:00Z"
          incremental_two_title="Routine issue two changed before cursor"
          incremental_two_updated="2026-07-31T23:00:00Z"
          ;;
        parity)
          incremental_one_title="Routine issue one updated after cursor"
          incremental_one_updated="2026-08-01T00:30:00Z"
          ;;
        closed)
          cat <<JSON
[{"number":2,"title":"$incremental_two_title","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"$incremental_two_updated","url":"https://example.invalid/issues/2"}]
JSON
          exit 0
          ;;
      esac
      cat <<JSON
[{"number":1,"title":"$incremental_one_title","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"$incremental_one_updated","url":"https://example.invalid/issues/1"},{"number":2,"title":"$incremental_two_title","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"$incremental_two_updated","url":"https://example.invalid/issues/2"}]
JSON
      ;;
    example-org/hub-repo)
      cat <<'JSON'
[{"number":14,"title":"spec(launch): public announcement","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/14"},{"number":15,"title":"spec(launch): installation guide","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/15"},{"number":16,"title":"spec(launch): release checklist","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/16"}]
JSON
      ;;
    example-org/capped)
      jq -cn '
        [
          range(1; 201)
          | . as $number
          | {
              number: $number,
              title: ("Capped issue " + ($number | tostring)),
              labels: [],
              createdAt: "2026-07-29T00:00:00Z",
              updatedAt: "2026-07-30T00:00:00Z",
              url: ("https://example.invalid/issues/" + ($number | tostring))
            }
        ]
      '
      ;;
    example-org/large-body)
      jq -cn '
        [{
          number: 1,
          title: "Large dependency report",
          body: (("x" * 204800) + "\nDepends on #123"),
          labels: [],
          createdAt: "2026-07-29T00:00:00Z",
          updatedAt: "2026-07-30T00:00:00Z",
          url: "https://example.invalid/issues/1"
        }]
      '
      ;;
    example-org/uncat-repo)
      uncat_title="Untriaged sample issue"
      if [ "${FAKE_GH_MODE:-base}" = "retitled" ]; then
        uncat_title="Untriaged sample issue (retitled)"
      fi
      cat <<JSON
[{"number":30,"title":"$uncat_title","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/30"}]
JSON
      ;;
    example-org/ci-drift-repo)
      if [ "${FAKE_GH_ISSUE_MODE:-none}" = "urgent" ]; then
        echo '[{"number":1,"title":"urgent: page the on-call","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/1"}]'
      else
        echo '[]'
      fi
      ;;
    example-org/landed-fix-repo)
      cat <<'JSON'
[{"number":301,"title":"bug: widget throws on empty input","body":"","labels":[{"name":"bug"}],"createdAt":"2026-07-01T00:00:00Z","updatedAt":"2026-07-01T00:00:00Z","url":"https://example.invalid/issues/301"}]
JSON
      ;;
    # #109: two different organisations, so a sweep.sh that mints only one
    # token for the whole run can read one repo and 404 the other.
    org-alpha/repo-one)
      echo '[{"number":1,"title":"chore: rotate the alpha widget","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/1"}]'
      ;;
    org-beta/repo-two)
      echo '[{"number":1,"title":"chore: rotate the beta widget","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/1"}]'
      ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "pr list" ]; then
  [ "${FAKE_GH_PR_FAIL:-0}" != "1" ] || exit 1
  case "$repo" in
    example-org/example-repo)
      pr8_title="fix: routine maintenance"
      if [ "${FAKE_GH_MODE:-base}" = "changed" ]; then
        pr8_title="fix: refreshed routine maintenance title"
      fi
      cat <<JSON
[{"number":8,"title":"$pr8_title","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/8","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[{"number":42,"labels":[{"name":"maintenance"}]}],"files":[{"path":"src/main.sh"}]},{"number":12,"title":"chore: update the frozen rule using a deliberately enormous descriptive title that cannot fit on one digest line without deterministic truncation","body":"BLOCKED BY example-org/another-repo#20.","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/12","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[],"files":[{"path":"rules/frozen-rules.md"}]},{"number":13,"labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/13","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"FAILURE","status":"COMPLETED"}],"closingIssuesReferences":[],"files":[]},{"number":16,"title":"docs: nested guide","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/16","isDraft":true,"reviewDecision":"","statusCheckRollup":[],"closingIssuesReferences":[],"files":[{"path":"docs/reference/deep/guide.md"}]}]
JSON
      ;;
    example-org/hub-repo)
      cat <<'JSON'
[{"number":18,"title":"spec(launch): public announcement","labels":[],"createdAt":"2026-07-30T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/18","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[{"number":14,"labels":[]}],"files":[]},{"number":17,"title":"spec(launch): installation guide","labels":[],"createdAt":"2026-07-30T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/17","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[{"number":15,"labels":[]}],"files":[]},{"number":19,"title":"spec(launch): release checklist","labels":[],"createdAt":"2026-07-30T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/19","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[{"number":16,"labels":[]}],"files":[]}]
JSON
      ;;
    example-org/replay-repo)
      cat <<'JSON'
[{"number":101,"title":"chore: rotate deploy workflow","labels":[],"url":"https://example.invalid/pull/101","baseRefName":"main","mergedAt":"2026-07-15T00:00:00Z","closingIssuesReferences":[],"files":[{"path":".github/workflows/deploy.yml"}]},{"number":102,"title":"chore: production deployment prep","labels":[],"url":"https://example.invalid/pull/102","baseRefName":"main","mergedAt":"2026-07-16T00:00:00Z","closingIssuesReferences":[],"files":[{"path":".github/workflows/publish.yml"}]},{"number":100,"title":"chore: ancient workflow tweak","labels":[],"url":"https://example.invalid/pull/100","baseRefName":"main","mergedAt":"2020-01-01T00:00:00Z","closingIssuesReferences":[],"files":[{"path":".github/workflows/old.yml"}]}]
JSON
      ;;
    example-org/audit-repo)
      cat <<'JSON'
[{"number":200,"mergedAt":"2020-01-01T00:00:00Z","headRefOid":"0000000000000000000000000000000000000000","mergeCommit":{"oid":"0000000000000000000000000000000000000001"}},{"number":201,"mergedAt":"2026-07-10T00:00:00Z","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","mergeCommit":{"oid":"1111111111111111111111111111111111111111"}},{"number":202,"mergedAt":"2026-07-11T00:00:00Z","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","mergeCommit":{"oid":"2222222222222222222222222222222222222222"}},{"number":203,"mergedAt":"2026-07-12T00:00:00Z","headRefOid":"cccccccccccccccccccccccccccccccccccccccc","mergeCommit":{"oid":"3333333333333333333333333333333333333333"}},{"number":204,"mergedAt":"2026-07-13T00:00:00Z","headRefOid":"dddddddddddddddddddddddddddddddddddddddd","mergeCommit":{"oid":"4444444444444444444444444444444444444444"}},{"number":205,"mergedAt":"2026-07-14T00:00:00Z","headRefOid":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","mergeCommit":{"oid":"5555555555555555555555555555555555555555"}},{"number":206,"mergedAt":"2026-07-15T00:00:00Z","headRefOid":"ffffffffffffffffffffffffffffffffffffffff","mergeCommit":{"oid":"6666666666666666666666666666666666666666"}},{"number":207,"mergedAt":"2026-07-16T00:00:00Z","headRefOid":"7777777777777777777777777777777777777777","mergeCommit":{"oid":"8888888888888888888888888888888888888888"}},{"number":208,"mergedAt":"2026-07-17T00:00:00Z","headRefOid":"abababababababababababababababababababab","mergeCommit":{"oid":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}}]
JSON
      ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "repo view" ]; then
  echo '{"defaultBranchRef":{"name":"main"}}'
  exit 0
fi
if [ "$1 $2" = "run list" ]; then
  case "$repo" in
    example-org/ci-drift-repo)
      case "${FAKE_GH_RUN_MODE:-red}" in
        red)
          echo '[{"databaseId":9001,"workflowDatabaseId":501,"workflowName":"Acceptance","name":"Acceptance","headSha":"cafefeed00000000","conclusion":"failure","status":"completed","createdAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/runs/9001"}]'
          ;;
        red-then-green)
          echo '[{"databaseId":9002,"workflowDatabaseId":501,"workflowName":"Acceptance","name":"Acceptance","headSha":"1111111100000000","conclusion":"success","status":"completed","createdAt":"2026-07-31T00:00:00Z","url":"https://example.invalid/runs/9002"},{"databaseId":9001,"workflowDatabaseId":501,"workflowName":"Acceptance","name":"Acceptance","headSha":"cafefeed00000000","conclusion":"failure","status":"completed","createdAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/runs/9001"}]'
          ;;
        no-workflows) echo '[]' ;;
        *) echo '[]' ;;
      esac
      ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1" = "api" ]; then
  shift
  # Find the endpoint (or "graphql") wherever it falls in the remaining
  # arguments, rather than assuming it is always the next one: gh api takes
  # flags like -X/-H/-f/-F/--jq before the endpoint, and callers are free to
  # add more of them (e.g. -X GET) without moving the endpoint's meaning.
  # Also track whether this call would trigger gh's real implicit-POST
  # switch: GET normally, but POST as soon as any -f/-F is present with no
  # explicit -X/--method. #86 was exactly that shape — a read issued as an
  # unmarked POST, 404ing on an endpoint that only exists for GET, with the
  # endpoint written *before* its -f flags (`gh api "$path" -f ...`). The
  # loop below must therefore keep scanning past the first positional token
  # rather than stopping there, or a -f that trails the endpoint — exactly
  # #86's shape — goes undetected and the regression this block exists to
  # catch would fail to catch it.
  endpoint=""
  method=""
  has_field=0
  if_none_match=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -X | --method)
        if [ "$#" -ge 2 ]; then method="$2"; shift 2; else shift; fi
        ;;
      -f | --field | -F | --raw-field)
        has_field=1
        if [ "$#" -ge 2 ]; then shift 2; else shift; fi
        ;;
      -H | --header)
        if [ "$#" -ge 2 ]; then
          case "$2" in
            If-None-Match:*) if_none_match="${2#If-None-Match: }" ;;
          esac
          shift 2
        else
          shift
        fi
        ;;
      --jq | --template)
        if [ "$#" -ge 2 ]; then shift 2; else shift; fi
        ;;
      -*)
        shift
        ;;
      *)
        [ -n "$endpoint" ] || endpoint="$1"
        shift
        ;;
    esac
  done
  if [ -z "$method" ] && [ "$has_field" = "1" ]; then
    echo '{"message":"Not Found","documentation_url":"https://docs.github.com/rest"}' >&2
    exit 1
  fi
  case "$endpoint" in
    repos/*/*/issues\?*)
      api_repo="${endpoint#repos/}"
      api_repo="${api_repo%%/issues\?*}"
      feed_kind=full
      case "$endpoint" in *'&since='*) feed_kind=incremental ;; esac
      fixture_etag="\"fixture-${api_repo//\//-}-$feed_kind-${FAKE_GH_MODE:-base}-${FAKE_GH_ISSUE_MODE:-none}\""
      if [ -n "$if_none_match" ] && [ "$if_none_match" = "$fixture_etag" ]; then
        printf 'HTTP/2.0 304 Not Modified\netag: %s\n\n' "$fixture_etag"
        exit 0
      fi

      query="${endpoint#*\?}"
      page=1
      since=""
      old_ifs="$IFS"
      IFS='&'
      for parameter in $query; do
        case "$parameter" in
          page=*) page="${parameter#page=}" ;;
          since=*) since="${parameter#since=}" ;;
        esac
      done
      IFS="$old_ifs"
      api_issues="$("$0" issue list --repo "$api_repo" --state open --limit 200)"
      if [ -n "$since" ]; then
        api_issues="$(
          jq -c --arg since "$since" \
            '[.[] | select((.updatedAt // .updated_at // "") > $since)]' \
            <<<"$api_issues"
        )"
      fi
      page_start=$(((page - 1) * 100))
      api_issues="$(jq -c --argjson start "$page_start" '.[$start:$start + 100]' <<<"$api_issues")"
      printf 'HTTP/2.0 200 OK\netag: %s\n\n%s\n' "$fixture_etag" "$api_issues"
      exit 0
      ;;
  esac
  case "$endpoint" in
    repos/example-org/landed-fix-repo/commits)
      # Emulates the shape `--jq` would already have reduced the raw GitHub
      # commit payload to: {sha, message, date}.
      cat <<'JSON'
[
  {"sha":"95d5ccc0deadbeef00000000000000000000000","message":"#301 GET /widgets 500: guard against nil pointer","date":"2026-07-05T00:00:00Z"},
  {"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","message":"unrelated change, predates the issue, also says #301","date":"2026-06-01T00:00:00Z"}
]
JSON
      ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
exit 1
EOF
chmod +x "$fixture/bin/gh"

# #106/#109: sweep.sh now authenticates as the gatekeeper App for its own
# `gh` calls instead of trusting whatever ambient token invoked it, minting
# one token per GitHub organisation in the roster rather than one for the
# whole run (#109: a single installation token cannot read a second
# organisation's repositories -- see sweep.sh's own comment on sweep_org).
# This curl fake is the network boundary for every sweep.sh test below, and
# it deliberately makes that org-scoping real rather than trivially true:
# the installation "id" it hands back is derived from the repository's own
# owner, and the token it later mints for that id embeds the same value.
# The gh fake below then refuses any call whose GH_TOKEN doesn't match the
# repository being queried -- so a test whose roster spans two
# organisations, but whose sweep.sh mistakenly mints only one token for
# the whole run, fails exactly the way a real second-organisation 404
# would, instead of silently mocking success everywhere.
cat >"$fixture/bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
url=""
for argument in "$@"; do
  case "$argument" in
    https://api.github.com/*) url="$argument" ;;
  esac
done
case "$url" in
  https://api.github.com/repos/*/installation)
    owner="${url#https://api.github.com/repos/}"
    owner="${owner%%/*}"
    id="$(printf '%s' "$owner" | cksum | awk '{print $1}')"
    printf '{"id":%s}\n200' "$id"
    ;;
  https://api.github.com/app/installations/*/access_tokens)
    id="${url#https://api.github.com/app/installations/}"
    id="${id%%/*}"
    printf '{"token":"stub-sweep-token-org-%s"}' "$id"
    ;;
  *) exit 99 ;;
esac
EOF
chmod +x "$fixture/bin/curl"
cat >"$fixture/bin/openssl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  base64)
    base64 | tr -d '\n'
    ;;
  dgst)
    cat >/dev/null
    printf 'stub-signature'
    ;;
  *) exit 99 ;;
esac
EOF
chmod +x "$fixture/bin/openssl"

# One shared placeholder private key: app-token.sh only checks that the path
# exists and is readable, since openssl itself is stubbed above.
gatekeeper_fixture_key="$fixture/gatekeeper-fixture.pem"
: >"$gatekeeper_fixture_key"

# Every mandate config directory a sweep.sh test points CLAUDE_CONFIG_DIR at
# needs its own gatekeeper credentials block, because MANDATE_DATA_DIR (and
# so secrets.yaml) follows CLAUDE_CONFIG_DIR and most fixtures below set it
# independently of one another.
write_gatekeeper_secrets() {
  mkdir -p "$1/ostrom"
  cat >"$1/ostrom/secrets.yaml" <<YAML
gatekeeper:
  app_id: 900100
  private_key_path: $gatekeeper_fixture_key
YAML
}
write_gatekeeper_secrets "$fixture/config"

# Local drift builds real repositories because patch equivalence, upstream
# absence, dirtiness, and linked-worktree discovery are Git behavior rather
# than string-parsing behavior.
local_drift="$fixture/local-drift"
mkdir -p "$local_drift/config/ostrom" "$local_drift/root"
git init --bare "$local_drift/origin.git" >/dev/null
git init -b main "$local_drift/root/repository" >/dev/null
git -C "$local_drift/root/repository" config user.name "Test User"
git -C "$local_drift/root/repository" config user.email "test@example.invalid"
printf '%s\n' base >"$local_drift/root/repository/tracked.txt"
git -C "$local_drift/root/repository" add tracked.txt
git -C "$local_drift/root/repository" commit -m "base" >/dev/null
git -C "$local_drift/root/repository" remote add origin "$local_drift/origin.git"
git -C "$local_drift/root/repository" push -u origin main >/dev/null
cat >"$local_drift/config/ostrom/mandates.yaml" <<YAML
search_roots:
  - $local_drift/root
bounce_all: []
projects: []
YAML

clean_local_drift="$(
  cd "$local_drift"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$local_drift/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/local-drift.sh"
)"
[ -z "$clean_local_drift" ]

# The landed branch has one raw commit whose patch was replayed onto main under
# a different SHA. git cherry must classify it as patch-equivalent debris.
git -C "$local_drift/root/repository" switch -c landed >/dev/null
printf '%s\n' landed >"$local_drift/root/repository/landed.txt"
git -C "$local_drift/root/repository" add landed.txt
git -C "$local_drift/root/repository" commit -m "landed patch" >/dev/null
landed_commit="$(git -C "$local_drift/root/repository" rev-parse landed)"
git -C "$local_drift/root/repository" switch main >/dev/null
printf '%s\n' main >"$local_drift/root/repository/main.txt"
git -C "$local_drift/root/repository" add main.txt
git -C "$local_drift/root/repository" commit -m "main divergence" >/dev/null
git -C "$local_drift/root/repository" cherry-pick "$landed_commit" >/dev/null
git -C "$local_drift/root/repository" push origin main >/dev/null

# The unpublished branch lives in a linked worktree outside the configured
# root and deliberately has no upstream. It is dirty as a separate condition.
git -C "$local_drift/root/repository" worktree add -b unpublished \
  "$local_drift/linked-worktree" origin/main >/dev/null
git -C "$local_drift/root/repository" config --unset-all branch.unpublished.remote \
  >/dev/null 2>&1 || true
git -C "$local_drift/root/repository" config --unset-all branch.unpublished.merge \
  >/dev/null 2>&1 || true
printf '%s\n' unpublished >"$local_drift/linked-worktree/unpublished.txt"
git -C "$local_drift/linked-worktree" add unpublished.txt
git -C "$local_drift/linked-worktree" commit -m "unpublished patch" >/dev/null
printf '%s\n' dirty >"$local_drift/linked-worktree/dirty.txt"

# A fully pushed branch with neither an open nor merged PR is the next-stage
# failure. The gh stub returns an empty list without using the network.
git -C "$local_drift/root/repository" switch -c pushed origin/main >/dev/null
printf '%s\n' pushed >"$local_drift/root/repository/pushed.txt"
git -C "$local_drift/root/repository" add pushed.txt
git -C "$local_drift/root/repository" commit -m "pushed patch" >/dev/null
git -C "$local_drift/root/repository" push -u origin pushed >/dev/null

local_drift_output="$(
  cd "$local_drift"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$local_drift/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/local-drift.sh"
)"
grep -Fq \
  'LIMIT: git cherry is patch-id based: it catches rebases but not squash merges; squash-merged work can appear unpublished.' \
  <<<"$local_drift_output"
grep -Eq $'^landed\t.*branch=landed\traw_commits=1\tpatches_not_in_main=0\t' \
  <<<"$local_drift_output"
grep -Eq $'^unpublished\t.*worktree=.*linked-worktree\tbranch=unpublished\traw_commits=1\tpatches_not_in_main=1\tpublication=unpushed-no-upstream$' \
  <<<"$local_drift_output"
grep -Eq $'^dirty\t.*worktree=.*linked-worktree\tbranch=unpublished\tchanges=1$' \
  <<<"$local_drift_output"
grep -Eq $'^unpublished\t.*branch=pushed\traw_commits=1\tpatches_not_in_main=1\tpublication=pushed-no-open-pr-or-merge$' \
  <<<"$local_drift_output"

unknown_pr_output="$(
  cd "$local_drift"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_PR_FAIL=1 \
    CLAUDE_CONFIG_DIR="$local_drift/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/local-drift.sh"
)"
grep -Eq $'^unpublished\t.*branch=pushed\t.*publication=pr-status-unknown$' \
  <<<"$unknown_pr_output"

# SessionStart keeps its established no-gh invariant. Its local-only scan still
# surfaces the unpushed work as exactly one detail-free digest line.
cat >"$local_drift/config/ostrom/state.json" <<'JSON'
{"version":2,"repos":{},"dead_selectors":[]}
JSON
: >"$local_drift/gh-calls"
local_drift_digest="$(
  cd "$local_drift"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$local_drift/gh-calls" \
    CLAUDE_CONFIG_DIR="$local_drift/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
local_drift_digest_text="$(jq -r '.systemMessage' <<<"$local_drift_digest")"
[ "$(grep -c '^LOCAL DRIFT — run mandate local-drift.sh for details$' \
  <<<"$local_drift_digest_text")" -eq 1 ]
[ ! -s "$local_drift/gh-calls" ]

run_sweep() {
  (
    cd "$fixture/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_CALL_LOG="$fixture/gh-calls" \
      FAKE_GH_MODE="${FAKE_GH_MODE:-base}" \
      MANDATE_SWEEP_MODE=full \
      CLAUDE_CONFIG_DIR="$fixture/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}

# #122: a policy change intentionally suppresses delegated rows for one
# sweep, but it must not advance past the events it withheld. First baseline
# the repository, then create a pending delegated row before changing both
# the item and a selector. The unchanged follow-up sweep must recover that
# same row from behind the held cursor.
policy_cursor="$fixture/policy-cursor"
mkdir -p "$policy_cursor/config/ostrom" "$policy_cursor/repo"
write_gatekeeper_secrets "$policy_cursor/config"
cat >"$policy_cursor/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/policy-cursor-repo
    delegated:
      - label:maintenance
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
run_policy_cursor_sweep() {
  (
    cd "$policy_cursor/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_MODE="${FAKE_GH_MODE:-base}" \
      MANDATE_SWEEP_MODE=full \
      CLAUDE_CONFIG_DIR="$policy_cursor/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}

run_policy_cursor_sweep >/dev/null
FAKE_GH_MODE=policy-pending run_policy_cursor_sweep >/dev/null
policy_cursor_queue="$policy_cursor/config/ostrom/queue.jsonl"
jq -s -e '
  length == 1
  and .[0].id == "example-org/policy-cursor-repo#1"
  and .[0].state == "pending"
' "$policy_cursor_queue" >/dev/null

sed -i '/    excluded: \[\]/c\    excluded:\n      - label:held' \
  "$policy_cursor/config/ostrom/mandates.yaml"
policy_cursor_suppressed="$(FAKE_GH_MODE=policy-changed run_policy_cursor_sweep)"
grep -q \
  'mandate sweep: 1 projects; 1 queue changes; 1 delegated row suppressed by mandate change' \
  <<<"$policy_cursor_suppressed"
if jq -e 'select(.id == "example-org/policy-cursor-repo#1")' \
  "$policy_cursor_queue" >/dev/null; then
  echo "policy change emitted a delegated row during its suppression sweep" >&2
  exit 1
fi

FAKE_GH_MODE=policy-changed run_policy_cursor_sweep >/dev/null
jq -s -e '
  length == 1
  and .[0].id == "example-org/policy-cursor-repo#1"
  and .[0].state == "pending"
' "$policy_cursor_queue" >/dev/null

# Issue bodies can exceed Linux's per-argument limit. The sweep must extract
# dependency refs without ever carrying the raw body through jq argv.
large_body="$fixture/large-body"
mkdir -p "$large_body/config/ostrom" "$large_body/repo"
write_gatekeeper_secrets "$large_body/config"
cat >"$large_body/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/large-body
    delegated: []
    excluded: []
    reserved:
      - 1
    default: excluded
    paused: false
    bounce: []
YAML
(
  cd "$large_body/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$large_body/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
jq -s -e '
  length == 1
  and .[0].id == "example-org/large-body#1"
  and .[0].blocked_by == ["example-org/large-body#123"]
' "$large_body/config/ostrom/queue.jsonl" >/dev/null

# #78: the sweep reads the default branch's own CI, not only open PRs'
# statusCheckRollup, and emits a "drift" row for it — the same kind and
# digest/troubled-project machinery a failing PR's CI already uses.
ci_drift="$fixture/ci-drift"
mkdir -p "$ci_drift/config/ostrom" "$ci_drift/repo"
write_gatekeeper_secrets "$ci_drift/config"
cat >"$ci_drift/config/ostrom/mandates.yaml" <<'YAML'
bounce_all:
  - title:*urgent*
projects:
  - repo: example-org/ci-drift-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML

run_ci_drift_sweep() {
  (
    cd "$ci_drift/repo"
    PATH="$fixture/bin:$PATH" \
      CLAUDE_CONFIG_DIR="$ci_drift/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      MANDATE_SWEEP_TIME="2026-08-01T00:00:00Z" \
      MANDATE_SWEEP_MODE=full \
      FAKE_GH_RUN_MODE="${FAKE_GH_RUN_MODE:-red}" \
      FAKE_GH_ISSUE_MODE="${FAKE_GH_ISSUE_MODE:-none}" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
  )
}

# A red default branch with no other activity: exactly one drift row, and it
# carries the workflow name, run id, head sha, and how long it has been red
# rather than making the builder re-derive any of that.
FAKE_GH_RUN_MODE=red run_ci_drift_sweep
jq -s -e '
  length == 1
  and .[0].id == "example-org/ci-drift-repo#501"
  and .[0].kind == "drift"
  and .[0].state == "pending"
  and (.[0].mandate.reason | contains("Acceptance"))
  and (.[0].mandate.reason | contains("run 9001"))
  and (.[0].mandate.reason | contains("cafefeed"))
  and (.[0].mandate.reason | contains("red since 2026-07-30T00:00:00Z"))
' "$ci_drift/config/ostrom/queue.jsonl" >/dev/null

# A default branch that failed and was then fixed by a later run must not
# read as drift: the row above must disappear, not linger from the prior
# sweep.
FAKE_GH_RUN_MODE=red-then-green run_ci_drift_sweep
[ "$(jq -s 'length' "$ci_drift/config/ostrom/queue.jsonl")" -eq 0 ]

# A repo with no workflow runs at all produces no row and no error — the
# absence of CI is not itself drift.
FAKE_GH_RUN_MODE=no-workflows run_ci_drift_sweep
[ "$(jq -s 'length' "$ci_drift/config/ostrom/queue.jsonl")" -eq 0 ]

# A red default branch alongside an unrelated troubled row: both surface: the
# drift row is not masked by, nor does it mask, the tripwire.
FAKE_GH_RUN_MODE=red FAKE_GH_ISSUE_MODE=urgent run_ci_drift_sweep
jq -s -e '
  length == 2
  and any(.[]; .kind == "drift" and .id == "example-org/ci-drift-repo#501")
  and any(.[]; .kind == "tripwire" and .id == "example-org/ci-drift-repo#1")
' "$ci_drift/config/ostrom/queue.jsonl" >/dev/null

# The digest already treats any "drift"-kind row as troubling a project; a
# red default branch reaches that machinery with no rendering changes.
ci_drift_digest="$(
  cd "$ci_drift/repo"
  PATH="$fixture/bin:$PATH" CLAUDE_CONFIG_DIR="$ci_drift/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_NOW_EPOCH=1785542400 MANDATE_TODAY=2026-08-01 \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
jq -r '.systemMessage' <<<"$ci_drift_digest" | grep -q '^0 projects nominal$'

# #77: an issue about to read (or already reading) "stuck" gets a lead when a
# default-branch commit references it by bare number after it was opened —
# never a verdict, and never on state or kind.
landed_fix="$fixture/landed-fix"
mkdir -p "$landed_fix/config/ostrom" "$landed_fix/repo"
write_gatekeeper_secrets "$landed_fix/config"
cat >"$landed_fix/config/ostrom/mandates.yaml" <<'YAML'
stuck_after_days: 1
bounce_all: []
projects:
  - repo: example-org/landed-fix-repo
    delegated:
      - label:bug
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
YAML
(
  cd "$landed_fix/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$landed_fix/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SWEEP_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
# Baseline sweep: not yet stuck, no lead expected.
jq -s -e 'length == 0' "$landed_fix/config/ostrom/queue.jsonl" >/dev/null

(
  cd "$landed_fix/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$landed_fix/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SWEEP_TIME="2026-08-03T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
# Now stuck. The lead names the earliest commit that both postdates the
# issue's opened date and references it without a closing keyword — the
# older commit in the stub predates the issue and must not be offered even
# though it also contains a bare "#301". state and kind are untouched: this
# is a pointer for the builder to verify, not an auto-close.
jq -s -e '
  length == 1
  and .[0].id == "example-org/landed-fix-repo#301"
  and .[0].kind == "stuck"
  and .[0].state == "pending"
  and (.[0].mandate.reason | endswith(
      "; possibly landed: 95d5ccc0 references #301 without a closing keyword"
    ))
  and (.[0].mandate.reason | contains("aaaaaaaa") | not)
' "$landed_fix/config/ostrom/queue.jsonl" >/dev/null

# The first sweep is a baseline. Only reserved, tripwire, and CI-failing
# carve-outs queue; the paused project's issue tripwire still fires. #13 is
# both unclassified and CI-failing: the unclassified branch of the kind
# ladder outranks CI-failing, so it surfaces as a decision ("no selector
# matched") rather than a drift row — an agent can't act on either fact
# until a human classifies it.
run_sweep >/dev/null
queue="$fixture/config/ostrom/queue.jsonl"
state="$fixture/config/ostrom/state.json"

jq -e '
  select(.id == "example-org/example-repo#10" and .kind == "decision")
  | .mandate.reason == "reserved ref:#10"
' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/example-repo#12" and .kind == "tripwire")
  | .mandate.reason == "tripwire: project bounce path:rules/frozen-rules.md"
  and (.mandate.dossier | has("question") and has("options_ruled_out")
    and has("recommended_action") and has("blast_radius"))
' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/example-repo#13" and .kind == "decision")
  | .mandate.reason
      == "no selector matched (default:unclassified); classification needed"
' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/example-repo#14" and .kind == "tripwire")
  | .mandate.reason == "tripwire: bounce_all title:*credential*"
' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/another-repo#20" and .kind == "tripwire")
  | .mandate.reason
  | contains("title:*production deployment*")
' "$queue" >/dev/null
[ "$(jq -s 'length' "$queue")" -eq 5 ]
jq -s -e '
  all(.[];
    has("title")
    and ((.title | type) == "string")
    and ((.title | length) > 0)
  )
  and any(.[];
    .id == "example-org/example-repo#13"
    and .title == "(title unavailable)"
  )
' "$queue" >/dev/null

# Decision-support fields use the injected sweep clock and only mechanical
# inputs. The threshold resolves from the layered config.
jq -s -e '
  all(.[];
    .age_days == 3
    and .aged_out == true
    and (.blocked_by | type) == "array"
  )
  and any(.[];
    .id == "example-org/example-repo#10"
    and .kind == "decision"
    and .needs_judgment == true
    and .blocked_by == ["example-org/example-repo#168"]
  )
  and any(.[];
    .id == "example-org/example-repo#14"
    and .blocked_by == []
  )
  and any(.[];
    .id == "example-org/example-repo#12"
    and .kind == "tripwire"
    and .needs_judgment == true
    and .blocked_by == ["example-org/another-repo#20"]
  )
  and any(.[];
    .id == "example-org/example-repo#13"
    and .kind == "decision"
    and .needs_judgment == true
    and .blocked_by == []
  )
' "$queue" >/dev/null

# /ostrom:desk reads the same titled records rather than making every number another
# lookup task.
desk_rows="$(
  CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/queue.sh" list
)"
jq -s -e '
  length == 5
  and all(.[];
    ((.title | type) == "string")
    and ((.title | length) > 0)
  )
' <<<"$desk_rows" >/dev/null

# A pre-brief queue remains valid and loadable without the additive facts.
legacy_config="$fixture/legacy-queue/config"
mkdir -p "$legacy_config/ostrom"
jq -c 'del(.age_days, .aged_out, .needs_judgment, .blocked_by)' "$queue" \
  >"$legacy_config/ostrom/queue.jsonl"
legacy_rows="$(
  CLAUDE_CONFIG_DIR="$legacy_config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/queue.sh" list
)"
jq -s -e '
  length == 5
  and all(.[];
    (has("age_days") | not)
    and (has("aged_out") | not)
    and (has("needs_judgment") | not)
    and (has("blocked_by") | not)
  )
' <<<"$legacy_rows" >/dev/null

# A legacy queue remains readable. Its one-time title enrichment upgrades the
# durable rows without manufacturing queue changes.
jq -c 'del(.title)' "$queue" >"$fixture/queue.legacy"
mv "$fixture/queue.legacy" "$queue"
migration_result="$(run_sweep)"
grep -q '0 queue changes$' <<<"$migration_result"
jq -s -e 'all(.[]; has("title") and ((.title | length) > 0))' \
  "$queue" >/dev/null

# Reserved wins over the delegated label on #10, excluded wins over delegated
# on #15, and excluded does not suppress the #14 tripwire. Issues never get
# path data while a PR path can span arbitrary depth with **.
jq -e '
  .repos["example-org/example-repo"].items
  | .["example-org/example-repo#10"].classification == "reserved"
  and .["example-org/example-repo#11"].classification == "unclassified"
  and .["example-org/example-repo#14"].classification == "tripwire"
  and .["example-org/example-repo#15"].classification == "excluded"
  and .["example-org/example-repo#16"].classification == "delegated"
  and .["example-org/example-repo#16"].matched_selector == "path:docs/**"
' "$state" >/dev/null

# An unlabeled PR inherits its closing issue label but baseline does not queue
# an ordinary ready decision.
jq -e '
  .repos["example-org/example-repo"].items
  | .["example-org/example-repo#8"].classification == "delegated"
  and .["example-org/example-repo#8"].matched_selector == "label:maintenance"
' "$state" >/dev/null
if jq -e 'select(.id == "example-org/example-repo#8")' "$queue" >/dev/null; then
  echo "ordinary baseline item was queued" >&2
  exit 1
fi

# Paused projects are queried for issues so their tripwires cannot be paused.
grep -q $'example-org/another-repo\tissue list' "$fixture/gh-calls"
grep -q $'example-org/another-repo\tpr list' "$fixture/gh-calls"
grep -q $'example-org/another-repo\tissue list .*--limit 200' \
  "$fixture/gh-calls"
grep -q $'example-org/another-repo\tpr list .*--limit 200' \
  "$fixture/gh-calls"

# Baseline time, not an old upstream update, starts the stuck clock.
jq -e '
  .repos["example-org/example-repo"].items["example-org/example-repo#7"]
  | .first_seen > .updated and .stuck == false
' "$state" >/dev/null

digest="$(
  cd "$fixture/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$fixture/gh-calls" \
    CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
jq -e . <<<"$digest" >/dev/null
jq -s -e '
  length == 1
  and ((.[0] | type) == "object")
  and (.[0] | has("systemMessage"))
  and ((.[0].systemMessage | type) == "string")
  and ((.[0].hookSpecificOutput | type) == "object")
  and (.[0].hookSpecificOutput | has("additionalContext"))
  and ((.[0].hookSpecificOutput.additionalContext | type) == "string")
  and .[0].systemMessage == .[0].hookSpecificOutput.additionalContext
  and .[0].hookSpecificOutput.hookEventName == "SessionStart"
' <<<"$digest" >/dev/null
digest_text="$(jq -r '.systemMessage' <<<"$digest")"
grep -q '^BRIEF$' <<<"$digest_text"
grep -q "^Produce today's /ostrom:brief now\." <<<"$digest_text"
[ -f "$fixture/config/ostrom/.tap-2026-08-01" ]
grep -q \
  '^example-org/example-repo#10  feat(tooling): owner gate — reserved ref:#10$' \
  <<<"$digest_text"
grep -q \
  '^example-org/example-repo#13  (title unavailable) — no selector matched (default:unclassified); classification needed$' \
  <<<"$digest_text"
grep -q \
  '^example-org/example-repo#14  Rotate credential safely — tripwire: bounce_all title:\*credential\*$' \
  <<<"$digest_text"
long_digest_row="$(
  grep '^example-org/example-repo#12  ' <<<"$digest_text"
)"
long_rendered_title="${long_digest_row#*#12  }"
long_rendered_title="${long_rendered_title%% — *}"
[ "${#long_rendered_title}" -ge 45 ]
grep -q '^chore: update the frozen rule.*…$' \
  <<<"$long_rendered_title"
long_rendered_reason="${long_digest_row#* — }"
[ "$long_rendered_reason" = \
  "tripwire: project bounce path:rules/frozen-rules.md" ]
[ "${#long_digest_row}" -gt 100 ]
grep -q '^example-org/example-repo: baselined 10 open items$' <<<"$digest_text"
grep -q '^example-org/another-repo: baselined 1 open items$' <<<"$digest_text"
grep -q '^example-org/example-repo: 3 unclassified — /ostrom:desk triage$' <<<"$digest_text"
if grep -Eq 'dead selector|unmatched in last sweep' <<<"$digest_text"; then
  echo "unmatched selectors leaked into the digest" >&2
  exit 1
fi

# The hook stays local, and a repeat sweep with no upstream movement is a
# serialized no-op.
hook_calls_before="$(wc -l <"$fixture/gh-calls")"
second_digest="$(
  CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
second_digest_text="$(jq -r '.systemMessage' <<<"$second_digest")"
if grep -q '^BRIEF$' <<<"$second_digest_text"; then
  echo "brief directive rendered more than once in one day" >&2
  exit 1
fi
hook_calls_after="$(wc -l <"$fixture/gh-calls")"
[ "$hook_calls_before" -eq "$hook_calls_after" ]
cp "$queue" "$fixture/queue.before"
cp "$state" "$fixture/state.before"
run_sweep >/dev/null
cmp "$fixture/queue.before" "$queue"
cmp "$fixture/state.before" "$state"
steady_digest="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
steady_digest_text="$(jq -r '.systemMessage' <<<"$steady_digest")"
if grep -q 'baselined [0-9][0-9]* open items' <<<"$steady_digest_text"; then
  echo "baseline notice survived an unchanged second sweep" >&2
  exit 1
fi

# Dependency prose is decision-support metadata, not material movement. Two
# otherwise identical items must retain the same fingerprint and emit no row.
fingerprint_before="$(
  jq -r '
    .repos["example-org/example-repo"].items
    | .["example-org/example-repo#7"].fingerprint
  ' "$state"
)"
dependency_result="$(FAKE_GH_MODE=dependency-changed run_sweep)"
fingerprint_after="$(
  jq -r '
    .repos["example-org/example-repo"].items
    | .["example-org/example-repo#7"].fingerprint
  ' "$state"
)"
[ "$fingerprint_before" = "$fingerprint_after" ]
grep -q '0 queue changes$' <<<"$dependency_result"
if jq -e '
  select(.id == "example-org/example-repo#7" and .kind == "moved")
' "$queue" >/dev/null; then
  echo "dependency-only edit generated a moved row" >&2
  exit 1
fi

# A title-only upstream change produces one refreshed ready decision, proving
# titles are not frozen and inherited labels reach the live classifier too.
FAKE_GH_MODE=changed run_sweep >/dev/null
jq -e '
  select(.id == "example-org/example-repo#7" and .kind == "moved")
  | .mandate.reason
      == "delegated scope:tooling; updated since the read cursor"
' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/example-repo#8" and .kind == "decision")
  | .title == "fix: refreshed routine maintenance title"
  and (.mandate.reason | startswith("delegated label:maintenance;"))
' "$queue" >/dev/null

# Existing moved rows written by 30eef35 regain the full stored reason even
# when no new upstream event regenerates them.
jq -c '
  if .id == "example-org/example-repo#7"
  then .mandate.reason |= sub("; updated since the read cursor$"; "")
  else .
  end
' "$queue" >"$fixture/queue.short-moved-reason"
mv "$fixture/queue.short-moved-reason" "$queue"
FAKE_GH_MODE=changed run_sweep >/dev/null
jq -e '
  select(.id == "example-org/example-repo#7")
  | .mandate.reason
      == "delegated scope:tooling; updated since the read cursor"
' "$queue" >/dev/null

refreshed_digest="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
refreshed_digest_text="$(jq -r '.systemMessage' <<<"$refreshed_digest")"
refreshed_row="$(grep '^example-org/example-repo#8  ' <<<"$refreshed_digest_text")"
[ "${#refreshed_row}" -le 100 ]
grep -q \
  '^example-org/example-repo#8  fix: refreshed routine maintenance title — delegated label:maintenance;.*…$' \
  <<<"$refreshed_row"
grep -q \
  '^example-org/example-repo#7  .* — delegated scope:tooling$' \
  <<<"$refreshed_digest_text"
if grep -q 'updated since the read cursor' <<<"$refreshed_digest_text"; then
  echo "moved-row heading was repeated in its reason" >&2
  exit 1
fi

# Editing a selector re-baselines scope: one item enters, one leaves, neither
# is emitted as a routine row, and the detail is durable for /ostrom:desk.
write_config "title:*Untriaged*"
run_sweep >/dev/null
if jq -e '
  select(.id == "example-org/example-repo#7"
    or .id == "example-org/example-repo#8")
' "$queue" >/dev/null; then
  echo "selector edit re-flooded routine rows" >&2
  exit 1
fi
jq -e '
  .repos["example-org/example-repo"]
  | .notice.text
      == "example-org/example-repo: mandate changed — 1 items entered scope, 1 left"
  and .scope_changes.entered == ["example-org/example-repo#9"]
  and .scope_changes.left == ["example-org/example-repo#8"]
' "$state" >/dev/null
policy_digest="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
policy_digest_text="$(jq -r '.systemMessage' <<<"$policy_digest")"
grep -q 'mandate changed — 1 items entered scope, 1 left' <<<"$policy_digest_text"
policy_digest_again="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
policy_digest_again_text="$(jq -r '.systemMessage' <<<"$policy_digest_again")"
if grep -q 'mandate changed —' <<<"$policy_digest_again_text"; then
  echo "mandate-change notice rendered more than once" >&2
  exit 1
fi

# Selectors that matched nowhere remain available only through explicit lint.
jq -e '
  .dead_selectors
  | any(.source == "bounce_all" and .selector == "title:*never fires*")
' "$state" >/dev/null
if grep -Eq 'dead selector|unmatched in last sweep' <<<"$policy_digest_text"; then
  echo "unmatched selectors leaked into the policy digest" >&2
  exit 1
fi
lint_output="$(
  CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/queue.sh" lint
)"
grep -q '^unmatched in last sweep — bounce_all title:\*never fires\*$' \
  <<<"$lint_output"

# Queue mutations remain compatible with selector reasons.
CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/queue.sh" approve 'example-org/example-repo#10' |
  grep -q 'approval token mandate:example-org/example-repo#10'
jq -e '
  select(.id == "example-org/example-repo#10" and .state == "approved")
' "$queue" >/dev/null

CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/queue.sh" reject 'example-org/example-repo#12' >/dev/null
if jq -e 'select(.id == "example-org/example-repo#12")' "$queue" >/dev/null; then
  echo "rejected row remained in queue" >&2
  exit 1
fi

# A rejection appends exactly one line to selector-events.jsonl, attributing
# the dismissal to the selector that produced the row. #12 was a tripwire
# matched via the project's bounce path selector.
events_file="$fixture/config/ostrom/selector-events.jsonl"
[ -f "$events_file" ]
[ "$(wc -l <"$events_file")" -eq 1 ]
jq -e '
  .id == "example-org/example-repo#12"
  and .decision == "reject"
  and .matched_selector == "path:rules/frozen-rules.md"
  and .classification == "tripwire"
  and ((.ts | type) == "string")
' "$events_file" >/dev/null

# Rejecting an item that matched no selector — #13 fell through to the
# project default — records that fact instead of being dropped. The
# sentinel "default:unclassified" is exactly what classify() already
# produces for a no-match; nothing new is invented for the event log.
CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/queue.sh" reject 'example-org/example-repo#13' >/dev/null
if jq -e 'select(.id == "example-org/example-repo#13")' "$queue" >/dev/null; then
  echo "rejected row remained in queue" >&2
  exit 1
fi
[ "$(wc -l <"$events_file")" -eq 2 ]
jq -s -e '
  any(.[];
    .id == "example-org/example-repo#13"
    and .decision == "reject"
    and .matched_selector == "default:unclassified"
    and .classification == "unclassified"
  )
' "$events_file" >/dev/null

# replay.sh per-selector report: state.json already has many classified
# items and selector-events.jsonl already has the two rejections above (one
# attributed to a selector, one attributed to no selector at all). Neither
# fixture repo's static PR data carries a mergedAt, so this run exercises
# only the report half, not the miss half.
replay_fixture_output="$(
  CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    PATH="$fixture/bin:$PATH" \
    MANDATE_REPLAY_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/replay.sh" 30
)"
grep -q 'lower bound' <<<"$replay_fixture_output"
grep -q '^  none flagged$' <<<"$replay_fixture_output"
printf '%s\n' "$replay_fixture_output" | grep -Fq \
  "$(printf 'content-derived\texample-org/example-repo\tproject bounce\tpath:rules/frozen-rules.md\t1\t1')"
grep -q '^Dismissals attributed to no selector (the project default fired instead): 1$' \
  <<<"$replay_fixture_output"
grep -q '^Unmatched irreversible-surface merges (misses, lower bound): 0$' \
  <<<"$replay_fixture_output"
if grep -Eqi 'accuracy|[0-9]%' <<<"$replay_fixture_output"; then
  echo "replay report collapsed the two error types into a single score" >&2
  exit 1
fi

# replay.sh miss detection, in a dedicated repo with three merged PRs. #101
# touches a workflow file and matches no bounce selector — a miss. #102
# touches a workflow file too, but its title matches the project's bounce
# selector, so the tripwire would have fired — not a miss. #100 would also
# be a miss but merged outside the lookback window, proving the window is
# honored rather than scanning full history.
replay_dir="$fixture/replay"
mkdir -p "$replay_dir/config/ostrom" "$replay_dir/repo"
cat >"$replay_dir/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/replay-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce:
      - title:*production deployment*
YAML
replay_miss_output="$(
  cd "$replay_dir/repo"
  CLAUDE_CONFIG_DIR="$replay_dir/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    PATH="$fixture/bin:$PATH" \
    MANDATE_REPLAY_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/replay.sh" 30
)"
grep -q 'example-org/replay-repo#101' <<<"$replay_miss_output"
if grep -q 'example-org/replay-repo#102' <<<"$replay_miss_output"; then
  echo "replay flagged a PR that matched a bounce selector" >&2
  exit 1
fi
if grep -q 'example-org/replay-repo#100' <<<"$replay_miss_output"; then
  echo "replay flagged a merged PR outside its lookback window" >&2
  exit 1
fi
grep -q 'workflow file: .github/workflows/deploy.yml' <<<"$replay_miss_output"
grep -q '^Unmatched irreversible-surface merges (misses, lower bound): 1$' \
  <<<"$replay_miss_output"

# audit.sh joins on the head SHA that actually merged, not merely PR identity.
# The eight in-window merges cover pass, pass-with-excuse, fail,
# inconclusive, no verdict, only a non-merged-SHA verdict, and only a null-SHA
# verdict. #200 and its null-SHA record are outside the window. #202 has both
# a null-SHA record and a usable pass, proving record-quality counts do not
# disappear merely because the PR can ultimately be joined. #208 carries two
# records at the same merged SHA, fail then pass — this is the regression
# fixture for the gate-log index (Fix 2): only the later record's verdict
# ("pass") may be reported, so a reordering bug in the index would flip the
# fail/pass counts below.
audit_dir="$fixture/audit"
audit_call_log="$fixture/audit-gh-calls.log"
mkdir -p "$audit_dir/config/ostrom" "$audit_dir/repo"
cat >"$audit_dir/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/audit-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
cat >"$audit_dir/config/ostrom/gate.jsonl" <<'JSONL'
{"ts":"2020-01-01T00:00:00Z","pr":"example-org/audit-repo#200","head_sha":null,"verdict":"inconclusive","already_judged":false,"conditions":[{"name":"mergeable","result":"inconclusive","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-09T00:00:00Z","pr":"example-org/audit-repo#201","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","verdict":"pass","already_judged":false,"conditions":[{"name":"mergeable","result":"pass","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-10T00:00:00Z","pr":"example-org/audit-repo#202","head_sha":null,"verdict":"inconclusive","already_judged":false,"conditions":[{"name":"mergeable","result":"inconclusive","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-10T01:00:00Z","pr":"example-org/audit-repo#202","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","verdict":"pass","already_judged":false,"conditions":[{"name":"reserved_refs","result":"excused","tier":["author-written"],"detail":{},"exception_reason":"principal accepted fixture"}]}
{"ts":"2026-07-11T00:00:00Z","pr":"example-org/audit-repo#203","head_sha":"cccccccccccccccccccccccccccccccccccccccc","verdict":"fail","already_judged":false,"conditions":[{"name":"mergeable","result":"pass","tier":["content-derived"],"detail":{}},{"name":"review_threads","result":"fail","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-13T00:00:00Z","pr":"example-org/audit-repo#205","head_sha":"9999999999999999999999999999999999999999","verdict":"fail","already_judged":false,"conditions":[{"name":"bounce_selectors","result":"fail","tier":["author-written","content-derived"],"detail":{}}]}
{"ts":"2026-07-14T00:00:00Z","pr":"example-org/audit-repo#206","head_sha":null,"verdict":"inconclusive","already_judged":false,"conditions":[{"name":"required_checks","result":"inconclusive","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-15T00:00:00Z","pr":"example-org/audit-repo#207","head_sha":"7777777777777777777777777777777777777777","verdict":"inconclusive","already_judged":false,"conditions":[{"name":"required_checks","result":"inconclusive","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-17T00:00:00Z","pr":"example-org/audit-repo#208","head_sha":"abababababababababababababababababababab","verdict":"fail","already_judged":false,"conditions":[{"name":"mergeable","result":"fail","tier":["content-derived"],"detail":{}}]}
{"ts":"2026-07-17T01:00:00Z","pr":"example-org/audit-repo#208","head_sha":"abababababababababababababababababababab","verdict":"pass","already_judged":false,"conditions":[{"name":"mergeable","result":"pass","tier":["content-derived"],"detail":{}}]}
JSONL
audit_sources_before="$(
  sha256sum "$audit_dir/config/ostrom/mandates.yaml" \
    "$audit_dir/config/ostrom/gate.jsonl"
)"
audit_output="$(
  cd "$audit_dir/repo"
  CLAUDE_CONFIG_DIR="$audit_dir/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$audit_call_log" \
    MANDATE_AUDIT_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/audit.sh" 30
)"
audit_sources_after="$(
  sha256sum "$audit_dir/config/ostrom/mandates.yaml" \
    "$audit_dir/config/ostrom/gate.jsonl"
)"
[ "$audit_sources_before" = "$audit_sources_after" ]
grep -Fxq "$(printf 'no verdict at any SHA\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'only null-SHA verdicts (unjoinable)\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'verdict exists, but none at the merged SHA\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'pass at the merged SHA\t3')" <<<"$audit_output"
grep -Fxq "$(printf 'of passes, contains an excused condition\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'fail or inconclusive at the merged SHA\t2')" <<<"$audit_output"
grep -Fxq "$(printf '  fail\t1')" <<<"$audit_output"
grep -Fxq "$(printf '  inconclusive\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'total merged PRs in window\t8')" <<<"$audit_output"
grep -Fxq "$(printf 'null head_sha records for merged PRs in window\t2')" \
  <<<"$audit_output"
grep -Fxq "$(printf 'merged PRs touched by a null head_sha record\t2')" \
  <<<"$audit_output"
grep -Fxq "$(printf 'review_threads\t1')" <<<"$audit_output"
grep -Fxq "$(printf 'required_checks\t1')" <<<"$audit_output"
if grep -q "$(printf '^bounce_selectors\t')" <<<"$audit_output"; then
  echo "audit attributed a non-merged-SHA failure to the fail bucket" >&2
  exit 1
fi
# #208's two same-SHA records must be resolved by the gate-log index in
# their original file order: the later "pass" record wins, not the earlier
# "fail" one. If the index (Fix 2) ever reordered records within a PR's
# group, #208 would report as fail instead, which would also throw off the
# fail/pass counts already asserted above.
if grep -Fq "$(printf 'example-org/audit-repo#208\tfail')" <<<"$audit_output"; then
  echo "audit picked the earlier gate record instead of the later one for #208" >&2
  exit 1
fi
grep -Fq "$(printf 'example-org/audit-repo#205\tnone at merged SHA\teeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\t5555555555555555555555555555555555555555\t0')" \
  <<<"$audit_output"
grep -Fq "$(printf 'example-org/audit-repo#206\tonly null-SHA verdicts\tffffffffffffffffffffffffffffffffffffffff\t6666666666666666666666666666666666666666\t1')" \
  <<<"$audit_output"
if grep -q 'example-org/audit-repo#200' <<<"$audit_output"; then
  echo "audit included a merged PR outside its lookback window" >&2
  exit 1
fi
[ "$(wc -l <"$audit_call_log" | tr -d '[:space:]')" -eq 2 ]
grep -Fxq -- $'-\tauth status --hostname github.com' "$audit_call_log"
grep -Fxq $'example-org/audit-repo\tpr list --repo example-org/audit-repo --state merged --limit 200 --json number,mergedAt,headRefOid,mergeCommit' \
  "$audit_call_log"
if grep -Eq '[0-9]+%' <<<"$audit_output"; then
  echo "audit collapsed separate evidence into a percentage" >&2
  exit 1
fi

# An absent gate log is a legitimate first-run state: the report must still
# be produced, but it must say so explicitly, so zero counts are never
# mistaken for a measurement.
absent_gate_dir="$fixture/audit-absent-gate"
mkdir -p "$absent_gate_dir/config/ostrom" "$absent_gate_dir/repo"
cat >"$absent_gate_dir/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/absent-gate-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
[ ! -e "$absent_gate_dir/config/ostrom/gate.jsonl" ]
absent_gate_output="$(
  cd "$absent_gate_dir/repo"
  CLAUDE_CONFIG_DIR="$absent_gate_dir/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    PATH="$fixture/bin:$PATH" \
    MANDATE_AUDIT_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/audit.sh" 30
)"
grep -Fq "Gate log is absent at" <<<"$absent_gate_output"
grep -Fxq "$(printf 'total merged PRs in window\t0')" <<<"$absent_gate_output"

# An empty gate log means the same thing as absent (no verdicts recorded
# yet) and must carry the same explicit notice.
empty_gate_dir="$fixture/audit-empty-gate"
mkdir -p "$empty_gate_dir/config/ostrom" "$empty_gate_dir/repo"
sed 's/absent-gate-repo/empty-gate-repo/' \
  "$absent_gate_dir/config/ostrom/mandates.yaml" \
  >"$empty_gate_dir/config/ostrom/mandates.yaml"
: >"$empty_gate_dir/config/ostrom/gate.jsonl"
empty_gate_output="$(
  cd "$empty_gate_dir/repo"
  CLAUDE_CONFIG_DIR="$empty_gate_dir/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    PATH="$fixture/bin:$PATH" \
    MANDATE_AUDIT_TIME="2026-08-01T00:00:00Z" \
    bash "$PLUGIN_ROOT/scripts/audit.sh" 30
)"
grep -Fq "Gate log is empty at" <<<"$empty_gate_output"

# A gate log that exists but cannot be read is a different situation
# entirely from absent/empty: it is an unknown, not a zero, so audit.sh
# must fail loudly rather than silently report no verdicts. chmod 000 does
# not block root from reading, so this assertion only means something for
# a non-root test run; skip it rather than fail spuriously under root.
if [ "$(id -u)" -eq 0 ]; then
  echo "mandate tests: skipping unreadable-gate-log assertion (running as root; chmod 000 does not block root reads)"
else
  unreadable_gate_dir="$fixture/audit-unreadable-gate"
  mkdir -p "$unreadable_gate_dir/config/ostrom" "$unreadable_gate_dir/repo"
  sed 's/absent-gate-repo/unreadable-gate-repo/' \
    "$absent_gate_dir/config/ostrom/mandates.yaml" \
    >"$unreadable_gate_dir/config/ostrom/mandates.yaml"
  unreadable_gate_log="$unreadable_gate_dir/config/ostrom/gate.jsonl"
  cat >"$unreadable_gate_log" <<'JSONL'
{"ts":"2026-07-01T00:00:00Z","pr":"example-org/unreadable-gate-repo#1","head_sha":"1111111111111111111111111111111111111111","verdict":"pass","already_judged":false,"conditions":[]}
JSONL
  chmod 000 "$unreadable_gate_log"
  set +e
  unreadable_gate_output="$(
    cd "$unreadable_gate_dir/repo"
    CLAUDE_CONFIG_DIR="$unreadable_gate_dir/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      PATH="$fixture/bin:$PATH" \
      MANDATE_AUDIT_TIME="2026-08-01T00:00:00Z" \
      bash "$PLUGIN_ROOT/scripts/audit.sh" 30 2>"$unreadable_gate_dir/stderr"
  )"
  unreadable_gate_status=$?
  set -e
  [ "$unreadable_gate_status" -ne 0 ]
  [ -z "$unreadable_gate_output" ]
  grep -q 'mandate audit:' "$unreadable_gate_dir/stderr"
  grep -q 'gate log' "$unreadable_gate_dir/stderr"
  chmod 644 "$unreadable_gate_log"
fi

# The fixture shape is three issues plus the three PRs that close them.
# Prefer the PRs, retain their recognizable titles, and name each collapsed
# issue in the falsifiability reason: six raw candidates become three rows.
dedup="$fixture/hub-repo"
mkdir -p "$dedup/config/ostrom" "$dedup/repo"
write_gatekeeper_secrets "$dedup/config"
cat >"$dedup/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/hub-repo
    delegated: []
    excluded: []
    reserved:
      - 14
      - 15
      - 16
    default: excluded
    paused: false
    bounce: []
YAML
(
  cd "$dedup/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$dedup/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
dedup_queue="$dedup/config/ostrom/queue.jsonl"
jq -s -e '
  length == 3
  and ([.[].id] | sort)
    == [
      "example-org/hub-repo#17",
      "example-org/hub-repo#18",
      "example-org/hub-repo#19"
    ]
  and all(.[];
    .kind == "decision"
    and (.title | startswith("spec(launch):"))
    and (.mandate.reason | test("^reserved ref:#[0-9]+ \\(closes #[0-9]+\\)$"))
  )
' "$dedup_queue" >/dev/null
jq -e '
  select(.id == "example-org/hub-repo#18")
  | .title == "spec(launch): public announcement"
  and .mandate.reason == "reserved ref:#14 (closes #14)"
' "$dedup_queue" >/dev/null
dedup_digest="$(
  cd "$dedup/repo"
  CLAUDE_CONFIG_DIR="$dedup/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
dedup_digest_text="$(jq -r '.systemMessage' <<<"$dedup_digest")"
[ "$(grep -c '^example-org/hub-repo#' <<<"$dedup_digest_text")" -eq 3 ]
if grep -Eq '^example-org/hub-repo#1[456]  ' <<<"$dedup_digest_text"; then
  echo "closing issues were rendered beside their pull requests" >&2
  exit 1
fi
grep -q \
  '^example-org/hub-repo#18  spec(launch): public announcement — reserved ref:#14 (closes #14)$' \
  <<<"$dedup_digest_text"

# #145: issue acquisition is incremental between daily reconciliations. The
# persisted normalized records are the merge base; PRs remain a full listing.
make_incremental_fixture() {
  incremental_root="$1"
  mkdir -p "$incremental_root/config/ostrom" "$incremental_root/repo"
  write_gatekeeper_secrets "$incremental_root/config"
  cat >"$incremental_root/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/incremental-repo
    delegated:
      - label:maintenance
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
  : >"$incremental_root/gh-calls"
}

run_incremental_fixture() {
  incremental_root="$1"
  incremental_mode="$2"
  incremental_time="$3"
  requested_mode="$4"
  (
    cd "$incremental_root/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_CALL_LOG="$incremental_root/gh-calls" \
      FAKE_GH_MODE="$incremental_mode" \
      MANDATE_SWEEP_TIME="$incremental_time" \
      MANDATE_SWEEP_MODE="$requested_mode" \
      CLAUDE_CONFIG_DIR="$incremental_root/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
  )
}

incremental="$fixture/incremental"
make_incremental_fixture "$incremental"
run_incremental_fixture "$incremental" base "2026-08-01T00:00:00Z" full
incremental_state="$incremental/config/ostrom/state.json"
incremental_queue="$incremental/config/ostrom/queue.jsonl"
jq -e '
  .sweep_mode == "full"
  and .last_full_reconciliation == "2026-08-01T00:00:00Z"
  and (.repos["example-org/incremental-repo"].etag | type) == "string"
  and (.repos["example-org/incremental-repo"].records | length) == 2
' "$incremental_state" >/dev/null
cp "$incremental_queue" "$incremental/queue.before-quiet"

# The first incremental pass uses the full pass's validator conditionally on
# a since-bounded URL. A real server may answer 200 because that URL is a
# different representation; either way the empty delta emits no queue row.
: >"$incremental/gh-calls"
run_incremental_fixture "$incremental" base "2026-08-01T00:10:00Z" auto
cmp "$incremental/queue.before-quiet" "$incremental_queue"
grep -q 'api -X GET --include -H If-None-Match:' "$incremental/gh-calls"
grep -q 'repos/example-org/incremental-repo/issues?.*since=2026-08-01T00:00:00Z' \
  "$incremental/gh-calls"
grep -q $'example-org/incremental-repo\tpr list ' "$incremental/gh-calls"
jq -e '.sweep_mode == "incremental"' "$incremental_state" >/dev/null

# Now the validator belongs to the exact incremental URL. The next quiet pass
# receives 304, retains both issue records, and still writes no new queue row.
cp "$incremental_state" "$incremental/state.before-304"
: >"$incremental/gh-calls"
run_incremental_fixture "$incremental" base "2026-08-01T00:20:00Z" auto
cmp "$incremental/queue.before-quiet" "$incremental_queue"
cmp "$incremental/state.before-304" "$incremental_state"
grep -q 'api -X GET --include -H If-None-Match:' "$incremental/gh-calls"

# A missing validator from a previous-version state is an unconditional read,
# not a load failure. The response repopulates the additive ETag field.
jq 'del(.repos["example-org/incremental-repo"].etag)' "$incremental_state" \
  >"$incremental/state-without-etag"
mv "$incremental/state-without-etag" "$incremental_state"
: >"$incremental/gh-calls"
run_incremental_fixture "$incremental" base "2026-08-01T00:25:00Z" incremental
if grep -q 'api -X GET --include -H If-None-Match:' "$incremental/gh-calls"; then
  echo "state without an ETag still issued a conditional request" >&2
  exit 1
fi
jq -e '(.repos["example-org/incremental-repo"].etag | type) == "string"' \
  "$incremental_state" >/dev/null

# Only the issue updated after the cursor is in the delta. The older update is
# absent and therefore cannot overwrite its retained normalized record.
run_incremental_fixture "$incremental" incremental-delta \
  "2026-08-01T01:00:00Z" auto
jq -e '
  .repos["example-org/incremental-repo"] as $repo
  | $repo.cursor == "2026-08-01T00:30:00Z"
  and $repo.records["example-org/incremental-repo#1"].title
    == "Routine issue one updated after cursor"
  and $repo.records["example-org/incremental-repo#2"].title
    == "Routine issue two"
  and $repo.items["example-org/incremental-repo#2"].updated
    == "2026-07-30T00:00:00Z"
' "$incremental_state" >/dev/null
jq -s -e '
  length == 1
  and .[0].id == "example-org/incremental-repo#1"
' "$incremental_queue" >/dev/null

# Leaving the open set is invisible to `since`, so the incremental pass keeps
# the issue. Once 24 hours have elapsed, auto selects a full reconciliation and
# removes both its item state and its pending queue row.
run_incremental_fixture "$incremental" closed "2026-08-01T02:00:00Z" auto
jq -e '
  .sweep_mode == "incremental"
  and (.repos["example-org/incremental-repo"].items
    | has("example-org/incremental-repo#1"))
' "$incremental_state" >/dev/null
run_incremental_fixture "$incremental" closed "2026-08-02T01:00:01Z" auto
jq -e '
  .sweep_mode == "full"
  and .last_full_reconciliation == "2026-08-02T01:00:01Z"
  and (.repos["example-org/incremental-repo"].items
    | has("example-org/incremental-repo#1") | not)
  and (.repos["example-org/incremental-repo"].records
    | has("example-org/incremental-repo#1") | not)
' "$incremental_state" >/dev/null
[ "$(jq -s 'length' "$incremental_queue")" -eq 0 ]

# Given the same truthful upstream fixture, delta merge and a complete listing
# converge on identical per-repository state and queue content.
parity_incremental="$fixture/parity-incremental"
parity_full="$fixture/parity-full"
make_incremental_fixture "$parity_incremental"
make_incremental_fixture "$parity_full"
run_incremental_fixture "$parity_incremental" base "2026-08-01T00:00:00Z" full
run_incremental_fixture "$parity_full" base "2026-08-01T00:00:00Z" full
run_incremental_fixture "$parity_incremental" parity "2026-08-01T01:00:00Z" incremental
run_incremental_fixture "$parity_full" parity "2026-08-01T01:00:00Z" full
jq '.repos["example-org/incremental-repo"] | del(.etag, .cursor, .previous_cursor)' \
  "$parity_incremental/config/ostrom/state.json" >"$parity_incremental/repo-state"
jq '.repos["example-org/incremental-repo"] | del(.etag, .cursor, .previous_cursor)' \
  "$parity_full/config/ostrom/state.json" >"$parity_full/repo-state"
cmp "$parity_incremental/repo-state" "$parity_full/repo-state"
cmp "$parity_incremental/config/ostrom/queue.jsonl" \
  "$parity_full/config/ostrom/queue.jsonl"

# Hitting either GitHub query limit is a loud fault, not a partial portfolio
# that can be mistaken for authoritative state.
capped="$fixture/capped"
mkdir -p "$capped/config/ostrom" "$capped/repo"
write_gatekeeper_secrets "$capped/config"
cat >"$capped/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/capped
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
set +e
(
  cd "$capped/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$capped/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null 2>"$capped/error"
)
capped_status=$?
set -e
[ "$capped_status" -eq 6 ]
grep -q \
  '^mandate sweep: issues change feed for example-org/capped reached query_limit 200; refusing a truncated sweep$' \
  "$capped/error"
[ ! -e "$capped/config/ostrom/state.json" ]
[ ! -e "$capped/config/ostrom/queue.jsonl" ]

# A representative eight-project first sweep remains a compact digest.
portfolio="$fixture/portfolio"
mkdir -p "$portfolio/config/ostrom" "$portfolio/repo"
write_gatekeeper_secrets "$portfolio/config"
cat >"$portfolio/config/ostrom/mandates.yaml" <<'YAML'
bounce_all:
  - title:*production deployment*
projects:
YAML
for number in 1 2 3 4 5 6 7 8; do
  {
    echo "  - repo: example-org/repo-$number"
    echo "    delegated: []"
    echo "    excluded: []"
    echo "    reserved: []"
    echo "    default: excluded"
    echo "    paused: false"
    echo "    bounce: []"
  } >>"$portfolio/config/ostrom/mandates.yaml"
done
(
  cd "$portfolio/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$portfolio/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
portfolio_digest="$(
  cd "$portfolio/repo"
  CLAUDE_CONFIG_DIR="$portfolio/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
portfolio_digest_text="$(jq -r '.systemMessage' <<<"$portfolio_digest")"
[ "$(wc -l <<<"$portfolio_digest_text")" -le 20 ]
grep -q '^8 projects nominal$' <<<"$portfolio_digest_text"
if grep -Eq 'dead selector|unmatched in last sweep' <<<"$portfolio_digest_text"; then
  echo "mostly unmatched roster rendered selector diagnostics" >&2
  exit 1
fi

# Baseline notices render once, survive unchanged sweeps as acknowledged
# state, and become fresh one-shot news after a repo state reset.
baseline_once="$fixture/baseline-once"
mkdir -p "$baseline_once/config/ostrom" "$baseline_once/repo"
write_gatekeeper_secrets "$baseline_once/config"
cat >"$baseline_once/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/rebaseline
    delegated: []
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
YAML
run_baseline_once_sweep() {
  (
    cd "$baseline_once/repo"
    PATH="$fixture/bin:$PATH" \
      CLAUDE_CONFIG_DIR="$baseline_once/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
  )
}
render_baseline_once() {
  (
    cd "$baseline_once/repo"
    CLAUDE_CONFIG_DIR="$baseline_once/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/hooks/render-digest.sh"
  )
}

run_baseline_once_sweep
baseline_state_mtime="$(
  stat -c %Y "$baseline_once/config/ostrom/state.json" 2>/dev/null ||
    stat -f %m "$baseline_once/config/ostrom/state.json"
)"
first_baseline_digest="$(render_baseline_once)"
first_baseline_digest_text="$(jq -r '.systemMessage' <<<"$first_baseline_digest")"
grep -q '^example-org/rebaseline: baselined 0 open items$' \
  <<<"$first_baseline_digest_text"
reported_state_mtime="$(
  stat -c %Y "$baseline_once/config/ostrom/state.json" 2>/dev/null ||
    stat -f %m "$baseline_once/config/ostrom/state.json"
)"
[ "$baseline_state_mtime" -eq "$reported_state_mtime" ]
jq -e '
  .repos["example-org/rebaseline"].notice.reported == true
' "$baseline_once/config/ostrom/state.json" >/dev/null

run_baseline_once_sweep
second_baseline_digest="$(render_baseline_once)"
second_baseline_digest_text="$(jq -r '.systemMessage' <<<"$second_baseline_digest")"
if grep -q 'baselined [0-9][0-9]* open items' <<<"$second_baseline_digest_text"; then
  echo "baseline notice rendered after an unchanged second sweep" >&2
  exit 1
fi

jq '.repos = {}' "$baseline_once/config/ostrom/state.json" \
  >"$baseline_once/config/ostrom/state.reset"
mv "$baseline_once/config/ostrom/state.reset" \
  "$baseline_once/config/ostrom/state.json"
run_baseline_once_sweep
reset_baseline_digest="$(render_baseline_once)"
reset_baseline_digest_text="$(jq -r '.systemMessage' <<<"$reset_baseline_digest")"
grep -q '^example-org/rebaseline: baselined 0 open items$' \
  <<<"$reset_baseline_digest_text"
reset_baseline_digest_again="$(render_baseline_once)"
reset_baseline_digest_again_text="$(
  jq -r '.systemMessage' <<<"$reset_baseline_digest_again"
)"
if grep -q 'baselined [0-9][0-9]* open items' \
  <<<"$reset_baseline_digest_again_text"; then
  echo "reset baseline notice rendered more than once" >&2
  exit 1
fi

# Baseline notices and unclassified rollups are informational: without an
# actionable queue row every configured project remains nominal.
rollup="$fixture/rollup-only"
mkdir -p "$rollup/config/ostrom" "$rollup/repo"
cat >"$rollup/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/rollup-one
    delegated: []
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
  - repo: example-org/rollup-two
    delegated: []
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
YAML
cat >"$rollup/config/ostrom/state.json" <<'JSON'
{
  "version": 2,
  "repos": {
    "example-org/rollup-one": {
      "cursor": "2026-07-30T00:00:00Z",
      "notice": {
        "kind": "baseline",
        "text": "example-org/rollup-one: baselined 3 open items"
      },
      "unclassified": 3
    },
    "example-org/rollup-two": {
      "cursor": "2026-07-30T00:00:00Z",
      "notice": {
        "kind": "baseline",
        "text": "example-org/rollup-two: baselined 2 open items"
      },
      "unclassified": 2
    }
  },
  "dead_selectors": [
    {
      "repo": "example-org/rollup-one",
      "source": "delegated",
      "selector": "label:nothing-open"
    }
  ]
}
JSON
rollup_digest="$(
  cd "$rollup/repo"
  CLAUDE_CONFIG_DIR="$rollup/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
rollup_digest_text="$(jq -r '.systemMessage' <<<"$rollup_digest")"
grep -q '^example-org/rollup-one: baselined 3 open items$' <<<"$rollup_digest_text"
grep -q '^example-org/rollup-two: baselined 2 open items$' <<<"$rollup_digest_text"
grep -q '^example-org/rollup-one: 3 unclassified — /ostrom:desk triage$' \
  <<<"$rollup_digest_text"
grep -q '^example-org/rollup-two: 2 unclassified — /ostrom:desk triage$' \
  <<<"$rollup_digest_text"
grep -q '^2 projects nominal$' <<<"$rollup_digest_text"
if grep -Eq 'dead selector|unmatched in last sweep' <<<"$rollup_digest_text"; then
  echo "rollup-only digest rendered selector diagnostics" >&2
  exit 1
fi

# A healthy durable portfolio renders exactly one nominal line.
mkdir -p "$fixture/healthy/config/ostrom" "$fixture/healthy/repo"
cat >"$fixture/healthy/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
  - repo: example-org/another-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: true
    bounce: []
YAML
cat >"$fixture/healthy/config/ostrom/state.json" <<'JSON'
{"version":2,"repos":{},"dead_selectors":[]}
JSON
healthy="$(
  cd "$fixture/healthy/repo"
  CLAUDE_CONFIG_DIR="$fixture/healthy/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
healthy_text="$(jq -r '.systemMessage' <<<"$healthy")"
grep -q '^BRIEF$' <<<"$healthy_text"
[ "$(grep -c '^[0-9][0-9]* projects nominal$' <<<"$healthy_text")" -eq 1 ]

touch -t 200001010000 "$fixture/healthy/config/ostrom/state.json"
stale_digest="$(
  cd "$fixture/healthy/repo"
  CLAUDE_CONFIG_DIR="$fixture/healthy/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
stale_digest_text="$(jq -r '.systemMessage' <<<"$stale_digest")"
[ "$(wc -l <<<"$stale_digest_text")" -eq 3 ]
grep -q '^DECISIONS TAKEN: nothing since your last read$' <<<"$stale_digest_text"
grep -q '^STALE — mandate sweep overdue$' <<<"$stale_digest_text"
grep -q '^2 projects nominal$' <<<"$stale_digest_text"

# Unclassified items are event-gated, not safety carve-outs: a dormant
# unclassified item stays invisible sweep after sweep, but the moment it
# moves (a title/fingerprint change) it surfaces as a decision — the
# invisibility the audit found is fixed without dumping the whole dormant
# backlog into one digest.
uncat="$fixture/uncat"
mkdir -p "$uncat/config/ostrom" "$uncat/repo"
write_gatekeeper_secrets "$uncat/config"
cat >"$uncat/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/uncat-repo
    delegated: []
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
YAML
run_uncat_sweep() {
  (
    cd "$uncat/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_MODE="${FAKE_GH_MODE:-base}" \
      MANDATE_SWEEP_MODE=full \
      CLAUDE_CONFIG_DIR="$uncat/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}
uncat_queue="$uncat/config/ostrom/queue.jsonl"

# Baseline sweep: unclassified is not a safety carve-out, so it does not
# queue even on the very first sweep.
run_uncat_sweep >/dev/null
jq -e '
  .repos["example-org/uncat-repo"].items["example-org/uncat-repo#30"]
    .classification == "unclassified"
' "$uncat/config/ostrom/state.json" >/dev/null
[ ! -s "$uncat_queue" ] || [ "$(jq -s 'length' "$uncat_queue")" -eq 0 ]

# A steady-state resweep with no upstream change is also silent: the
# event-gated branch requires an event, and there is none.
run_uncat_sweep >/dev/null
[ ! -s "$uncat_queue" ] || [ "$(jq -s 'length' "$uncat_queue")" -eq 0 ]

# A title change is a fingerprint event: the item now surfaces as a
# decision, since nobody has said whether an agent may act on it.
FAKE_GH_MODE=retitled run_uncat_sweep >/dev/null
jq -e '
  select(.id == "example-org/uncat-repo#30")
  | .kind == "decision"
  and .needs_judgment == true
  and .mandate.reason
    == "no selector matched (default:unclassified); classification needed"
' "$uncat_queue" >/dev/null
[ "$(jq -s 'length' "$uncat_queue")" -eq 1 ]

empty="$fixture/empty-config"
mkdir -p "$empty"
unconfigured_stdout="$fixture/unconfigured.stdout"
set +e
(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$empty" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
) >"$unconfigured_stdout"
unconfigured_status=$?
set -e
[ "$unconfigured_status" -eq 0 ]
[ ! -s "$unconfigured_stdout" ]

# #80's transparency half: the digest leads with decisions taken since the
# principal's last read, newest first, each carrying its reversal pointer.
# "Since last read" is a small local watermark file, not a rewrite of the
# append-only trace.
decisions="$fixture/decisions"
mkdir -p "$decisions/config/ostrom" "$decisions/repo"
write_gatekeeper_secrets "$decisions/config"
cat >"$decisions/config/ostrom/mandates.yaml" <<'YAML'
provider: file
cadence_hours: 24
stuck_after_days: 1
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated: []
    excluded: []
    reserved:
      - 10
    default: excluded
    paused: false
    bounce: []
YAML
cat >"$decisions/config/ostrom/state.json" <<'JSON'
{"version":2,"repos":{},"dead_selectors":[]}
JSON
run_decisions_digest() {
  (
    cd "$decisions/repo"
    CLAUDE_CONFIG_DIR="$decisions/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      MANDATE_TODAY="2026-08-01" \
      "$@" bash "$PLUGIN_ROOT/hooks/render-digest.sh"
  )
}

# A trace with no decision-taken rows renders the one-line plain form, never
# an empty section.
none_yet_digest="$(run_decisions_digest env MANDATE_DIGEST_TIME="2026-08-01T00:00:00Z")"
none_yet_text="$(jq -r '.systemMessage' <<<"$none_yet_digest")"
grep -q '^DECISIONS TAKEN: nothing since your last read$' <<<"$none_yet_text"
if grep -q '^DECISIONS TAKEN$' <<<"$none_yet_text"; then
  echo "an empty decisions section rendered a heading with no rows" >&2
  exit 1
fi

# Two decision-taken records land after the watermark set by the render
# above. One is missing its reversal field entirely — it must degrade rather
# than take the hook down with it.
MANDATE_TRACE_TIME="2026-08-01T01:00:00Z" CLAUDE_CONFIG_DIR="$decisions/config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append decision-taken \
  '{"role":"gatekeeper","owner":"gatekeeper-t-1","repo":"example-org/example-repo","ref":"#42","decision":"merged pull request","reversal":"revert example-org/example-repo#42: open a revert pull request or git revert its merge commit"}' \
  '{"reason":"gate verdict: pass"}' >/dev/null
MANDATE_TRACE_TIME="2026-08-01T01:05:00Z" CLAUDE_CONFIG_DIR="$decisions/config" \
  bash "$PLUGIN_ROOT/scripts/trace.sh" append decision-taken \
  '{"role":"builder","owner":"builder-t-1","repo":"example-org/example-repo","ref":"#43","decision":"filed issue"}' \
  '{}' >/dev/null

# A pending tripwire keeps the queue non-empty so the still-blocked section
# has something to render below the new decisions.
run_sweep_for_decisions() {
  (
    cd "$decisions/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_CALL_LOG="$decisions/gh-calls" \
      CLAUDE_CONFIG_DIR="$decisions/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}
run_sweep_for_decisions >/dev/null

with_decisions_digest="$(
  run_decisions_digest env MANDATE_DIGEST_TIME="2026-08-01T02:00:00Z"
)"
with_decisions_text="$(jq -r '.systemMessage' <<<"$with_decisions_digest")"
jq -e . <<<"$with_decisions_digest" >/dev/null
grep -q '^DECISIONS TAKEN$' <<<"$with_decisions_text"
grep -q \
  '^example-org/example-repo#43  filed issue  \[reversal: reversal not recorded\]$' \
  <<<"$with_decisions_text"
grep -q \
  '^example-org/example-repo#42  merged pull request — gate verdict: pass  \[reversal: revert example-org/example-repo#42: open a revert pull request or git revert its merge commit\]$' \
  <<<"$with_decisions_text"
decisions_line_43="$(grep -n '^example-org/example-repo#43' <<<"$with_decisions_text" | cut -d: -f1)"
decisions_line_42="$(grep -n '^example-org/example-repo#42' <<<"$with_decisions_text" | cut -d: -f1)"
[ "$decisions_line_43" -lt "$decisions_line_42" ]
blocked_line="$(
  grep -n '^example-org/example-repo#10  ' <<<"$with_decisions_text" | cut -d: -f1
)"
[ -n "$blocked_line" ]
[ "$decisions_line_42" -lt "$blocked_line" ]

# Reading the digest again with no new decisions and a later watermark
# suppresses the ones already shown, proving the watermark advances.
rerun_digest="$(run_decisions_digest env MANDATE_DIGEST_TIME="2026-08-01T03:00:00Z")"
rerun_text="$(jq -r '.systemMessage' <<<"$rerun_digest")"
grep -q '^DECISIONS TAKEN: nothing since your last read$' <<<"$rerun_text"
if grep -q 'example-org/example-repo#42\|example-org/example-repo#43' \
  <<<"$rerun_text"; then
  echo "a decision already shown once rendered again after the watermark advanced" >&2
  exit 1
fi

# A malformed line elsewhere in the trace must never crash the hook: the
# digest still degrades to a shorter one, not an empty SessionStart.
printf 'not-json-at-all\n' >>"$decisions/config/ostrom/sprint.jsonl"
set +e
malformed_trace_digest="$(
  run_decisions_digest env MANDATE_DIGEST_TIME="2026-08-01T04:00:00Z" 2>"$decisions/stderr"
)"
malformed_trace_status=$?
set -e
[ "$malformed_trace_status" -eq 0 ]
jq -e . <<<"$malformed_trace_digest" >/dev/null
malformed_trace_text="$(jq -r '.systemMessage' <<<"$malformed_trace_digest")"
grep -q '^DECISIONS TAKEN: nothing since your last read$' <<<"$malformed_trace_text"

set +e
missing_message="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$empty" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" 2>&1
)"
missing_status=$?
set -e
[ "$missing_status" -eq 2 ]
grep -q 'no mandates.yaml found' <<<"$missing_message"

set +e
auth_message="$(
  cd "$fixture/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_AUTH_FAIL=1 \
    CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" 2>&1
)"
auth_status=$?
set -e
[ "$auth_status" -eq 3 ]
grep -q 'gh reports not authenticated' <<<"$auth_message"
grep -q 'never falls back to an ambient credential' <<<"$auth_message"

# #106's central property: a missing gatekeeper credential must fail loudly,
# with no queue write at all, never a silently empty-but-healthy queue --
# the exact shape that hid a red default branch for two and a half days
# (#78). An ambient GH_TOKEN/GITHUB_TOKEN in the invoking environment must
# never rescue this: the sweep mints its own token or it does not run.
# Two distinct missing-credential shapes, both covered: no secrets.yaml at
# all, and a secrets.yaml that configures some other role but has no shared
# fallback -- the sweep must name its own role, gatekeeper, and refuse rather
# than silently borrow another role's block.
no_credential="$fixture/no-credential"
mkdir -p "$no_credential/config/ostrom" "$no_credential/repo"
cat >"$no_credential/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: example-org/no-credential-repo
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
YAML
set +e
no_credential_message="$(
  cd "$no_credential/repo"
  PATH="$fixture/bin:$PATH" \
    GH_TOKEN="ambient-principal-value" \
    GITHUB_TOKEN="ambient-principal-value" \
    CLAUDE_CONFIG_DIR="$no_credential/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" 2>&1
)"
no_credential_status=$?
set -e
[ "$no_credential_status" -eq 111 ]
grep -q 'secrets file is missing' <<<"$no_credential_message"
grep -q 'could not mint a gatekeeper token' <<<"$no_credential_message"
! grep -q 'ambient-principal-value' <<<"$no_credential_message"
[ ! -e "$no_credential/config/ostrom/queue.jsonl" ]

other_role_only="$fixture/other-role-only"
mkdir -p "$other_role_only/config/ostrom" "$other_role_only/repo"
cp "$no_credential/config/ostrom/mandates.yaml" \
  "$other_role_only/config/ostrom/mandates.yaml"
cat >"$other_role_only/config/ostrom/secrets.yaml" <<YAML
builder:
  app_id: 900200
  private_key_path: $gatekeeper_fixture_key
YAML
set +e
other_role_message="$(
  cd "$other_role_only/repo"
  PATH="$fixture/bin:$PATH" \
    GH_TOKEN="ambient-principal-value" \
    GITHUB_TOKEN="ambient-principal-value" \
    CLAUDE_CONFIG_DIR="$other_role_only/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" 2>&1
)"
other_role_status=$?
set -e
[ "$other_role_status" -eq 111 ]
grep -q 'neither gatekeeper nor shared credentials are configured' \
  <<<"$other_role_message"
grep -q 'could not mint a gatekeeper token' <<<"$other_role_message"
! grep -q 'ambient-principal-value' <<<"$other_role_message"
[ ! -e "$other_role_only/config/ostrom/queue.jsonl" ]

# #109: a roster spanning two GitHub organisations. A single whole-run token
# minted off the first project's repo authenticates for one installation
# only; the curl/gh fakes above turn a second organisation's repositories
# reading under that wrong token into a hard failure, the same shape as a
# real cross-organisation 404. Before the fix this either aborted the sweep
# entirely or (worse) let org-beta's repository silently read as empty --
# either way losing an entire organisation from the queue while the sweep
# reported success. A single-organisation fixture cannot exercise this: it
# is only ever wrong once a second installation enters the picture.
cross_org="$fixture/cross-org"
mkdir -p "$cross_org/config/ostrom" "$cross_org/repo"
write_gatekeeper_secrets "$cross_org/config"
cat >"$cross_org/config/ostrom/mandates.yaml" <<'YAML'
bounce_all: []
projects:
  - repo: org-alpha/repo-one
    delegated: []
    excluded: []
    reserved:
      - 1
    default: excluded
    paused: false
    bounce: []
  - repo: org-beta/repo-two
    delegated: []
    excluded: []
    reserved:
      - 1
    default: excluded
    paused: false
    bounce: []
YAML
cross_org_calls="$cross_org/gh-calls"
: >"$cross_org_calls"
(
  cd "$cross_org/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$cross_org_calls" \
    CLAUDE_CONFIG_DIR="$cross_org/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
# Both organisations' repositories were actually queried under a working
# token, not just carried forward from a stale queue.
grep -q '^org-alpha/repo-one	issue list' "$cross_org_calls"
grep -q '^org-beta/repo-two	issue list' "$cross_org_calls"
jq -s -e '
  (map(.id) | index("org-alpha/repo-one#1")) != null
  and (map(.id) | index("org-beta/repo-two#1")) != null
' "$cross_org/config/ostrom/queue.jsonl" >/dev/null

# gate.yaml uses the mandate subsystem's shipped < user < repo layering while
# keeping its project schema separate from the private mandate roster.
gate_layers="$fixture/gate-layers"
mkdir -p "$gate_layers/config/ostrom" "$gate_layers/repo/.ostrom"
cat >"$gate_layers/config/ostrom/gate.yaml" <<'YAML'
provider: file
bounce_all: []
projects:
  - repo: placeholder-org/placeholder-repo
    required_checks:
      - verify-*
    bounce:
      - path:protected/**
    reserved:
      - 41
YAML
cat >"$gate_layers/repo/.ostrom/gate.yaml" <<'YAML'
bounce_all:
  - title:*principal review*
YAML
gate_layered="$(
  cd "$gate_layers/repo"
  CLAUDE_CONFIG_DIR="$gate_layers/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash -c 'source "$CLAUDE_PLUGIN_ROOT/scripts/mandate-lib.sh"; mandate_load_gate_config'
)"
jq -e '
  .provider == "file"
  and .bounce_all == ["title:*principal review*"]
  and .projects[0].repo == "placeholder-org/placeholder-repo"
  and .projects[0].required_checks == ["verify-*"]
  and .projects[0].bounce == ["path:protected/**"]
  and .projects[0].reserved == [41]
' <<<"$gate_layered" >/dev/null

# The merge gate has a dedicated gh stub. No gate test can reach the network.
gate_fixture="$fixture/gate"
mkdir -p "$gate_fixture/config/ostrom" "$gate_fixture/repo" "$gate_fixture/bin"
cat >"$gate_fixture/config/ostrom/gate.yaml" <<'YAML'
provider: file
bounce_all: []
projects:
  - repo: placeholder-org/placeholder-repo
    required_checks:
      - verify-*
    bounce:
      - title:*release*
      - path:.github/workflows/**
    reserved:
      - 99
YAML
cat >"$gate_fixture/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

if [ "$1 $2" = "pr view" ]; then
  if [ "${FAKE_GATE_MODE:-pass}" = "unresolvable-pr" ]; then
    echo "placeholder pull request could not be resolved" >&2
    exit 1
  fi
  check_conclusion="SUCCESS"
  if [ "${FAKE_GATE_MODE:-pass}" = "unknown-check" ]; then
    check_conclusion="UNRECOGNIZED"
  fi
  mergeable="MERGEABLE"
  is_draft=false
  if [ "${FAKE_GATE_MODE:-pass}" = "conflicting" ]; then
    mergeable="CONFLICTING"
  elif [ "${FAKE_GATE_MODE:-pass}" = "unknown-mergeable" ]; then
    mergeable="UNKNOWN"
  elif [ "${FAKE_GATE_MODE:-pass}" = "draft" ]; then
    is_draft=true
  fi
  jq -cn \
    --argjson number "$3" \
    --arg head "${FAKE_GATE_HEAD:-aaaaaaaaaaaaaaaa}" \
    --arg conclusion "$check_conclusion" \
    --arg mergeable "$mergeable" \
    --argjson is_draft "$is_draft" \
    --arg mode "${FAKE_GATE_MODE:-pass}" \
    --arg title "$(
      if [ "${FAKE_GATE_MODE:-pass}" = "tier" ]; then
        printf '%s' 'release: publish placeholder artifact'
      else
        printf '%s' 'fix(core): safe placeholder change'
      fi
    )" '
      {
        number: $number,
        title: $title,
        author: {login: "builder-login"},
        headRefOid: $head,
        labels: [],
        statusCheckRollup: [{
          name: "verify-linux",
          status: "COMPLETED",
          conclusion: $conclusion
        }],
        closingIssuesReferences: [],
        mergeable: $mergeable,
        isDraft: $is_draft
      }
      | if $mode == "missing-mergeable" then del(.mergeable)
        elif $mode == "malformed-mergeable" then .mergeable = 7
        else .
        end
    '
  exit 0
fi
if [ "$1 $2" = "pr diff" ]; then
  if [ "${FAKE_GATE_MODE:-pass}" = "tier" ]; then
    printf '%s\n' '.github/workflows/placeholder.yml'
  else
    printf '%s\n' 'src/placeholder.sh'
  fi
  exit 0
fi
if [ "$1 $2" = "api graphql" ]; then
  if [ "${FAKE_GATE_MODE:-pass}" = "thread-author" ]; then
    cat <<'JSON'
{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":[{"id":"THREAD_placeholder","isResolved":true,"resolvedBy":{"login":"builder-login"},"comments":{"nodes":[{"author":{"login":"placeholder-reviewer-bot"}}]}}],"pageInfo":{"hasNextPage":false,"endCursor":"cursor-placeholder"}}}}}}
JSON
  elif [ "${FAKE_GATE_MODE:-pass}" = "thread-unanswered" ]; then
    cat <<'JSON'
{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":[{"id":"THREAD_placeholder","isResolved":false,"resolvedBy":null,"comments":{"nodes":[{"author":{"login":"placeholder-reviewer-bot"}}]}}],"pageInfo":{"hasNextPage":false,"endCursor":"cursor-placeholder"}}}}}}
JSON
  elif [ "${FAKE_GATE_MODE:-pass}" = "thread-answered" ]; then
    cat <<'JSON'
{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":[{"id":"THREAD_placeholder","isResolved":false,"resolvedBy":null,"comments":{"nodes":[{"author":{"login":"builder-login"}}]}}],"pageInfo":{"hasNextPage":false,"endCursor":"cursor-placeholder"}}}}}}
JSON
  else
    cat <<'JSON'
{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"cursor-placeholder"}}}}}}
JSON
  fi
  exit 0
fi
exit 1
EOF
chmod +x "$gate_fixture/bin/gh"

# The exception verb rejects malformed authority and resolves the PR head
# itself before appending one complete JSON object.
excuse_log="$gate_fixture/config/ostrom/exceptions.jsonl"
set +e
excuse_message="$(
  PATH="$gate_fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      not-a-pr bounce_selectors "placeholder reason" 2>&1
)"
excuse_status=$?
set -e
[ "$excuse_status" -eq 2 ]
grep -q '^usage: excuse.sh grant ' <<<"$excuse_message"
[ ! -e "$excuse_log" ]

set +e
excuse_message="$(
  PATH="$gate_fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      placeholder-org/placeholder-repo#7 unknown "placeholder reason" 2>&1
)"
excuse_status=$?
set -e
[ "$excuse_status" -eq 2 ]
grep -q 'required_checks, review_threads, bounce_selectors, reserved_refs' \
  <<<"$excuse_message"
[ ! -e "$excuse_log" ]

set +e
excuse_message="$(
  PATH="$gate_fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      placeholder-org/placeholder-repo#7 bounce_selectors '   ' 2>&1
)"
excuse_status=$?
set -e
[ "$excuse_status" -eq 2 ]
grep -q 'reason must not be empty' <<<"$excuse_message"
[ ! -e "$excuse_log" ]

set +e
excuse_message="$(
  PATH="$gate_fixture/bin:$PATH" \
    FAKE_GATE_MODE=unresolvable-pr \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      placeholder-org/placeholder-repo#7 bounce_selectors \
      "placeholder reason" 2>&1
)"
excuse_status=$?
set -e
[ "$excuse_status" -eq 3 ]
grep -q 'could not resolve placeholder-org/placeholder-repo#7' \
  <<<"$excuse_message"
[ ! -e "$excuse_log" ]

granted_head="1111111111111111111111111111111111111111"
grant_output="$(
  PATH="$gate_fixture/bin:$PATH" \
    FAKE_GATE_HEAD="$granted_head" \
    MANDATE_EXCUSE_TIME="2026-08-04T11:00:00Z" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      placeholder-org/placeholder-repo#7 bounce_selectors \
      "principal accepted placeholder surface"
)"
jq -e --arg head "$granted_head" '
  . == {
    ts: "2026-08-04T11:00:00Z",
    repo: "placeholder-org/placeholder-repo",
    pr: 7,
    head_sha: $head,
    condition: "bounce_selectors",
    reason: "principal accepted placeholder surface"
  }
' <<<"$grant_output" >/dev/null
[ "$(wc -l <"$excuse_log" | tr -d '[:space:]')" -eq 1 ]

# A SHA in the caller-supplied condition position is rejected; grant has no
# SHA argument and always takes it from gh pr view.
set +e
PATH="$gate_fixture/bin:$PATH" \
  CLAUDE_CONFIG_DIR="$gate_fixture/config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
    placeholder-org/placeholder-repo#7 "$granted_head" bounce_selectors \
    "placeholder reason" >/dev/null 2>&1
excuse_status=$?
set -e
[ "$excuse_status" -eq 2 ]
[ "$(wc -l <"$excuse_log" | tr -d '[:space:]')" -eq 1 ]

list_output="$(
  PATH="$gate_fixture/bin:$PATH" \
    FAKE_GATE_HEAD="$granted_head" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" list \
      placeholder-org/placeholder-repo#7
)"
grep -q '^current placeholder-org/placeholder-repo#7 bounce_selectors ' \
  <<<"$list_output"
grep -q 'reason="principal accepted placeholder surface"$' <<<"$list_output"
list_output="$(
  PATH="$gate_fixture/bin:$PATH" \
    FAKE_GATE_HEAD="7777777777777777777777777777777777777777" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" list \
      placeholder-org/placeholder-repo#7
)"
grep -q '^superseded placeholder-org/placeholder-repo#7 bounce_selectors ' \
  <<<"$list_output"

run_gate() {
  gate_mode="$1"
  gate_number="$2"
  gate_head="$3"
  gate_output_file="$gate_fixture/$gate_mode-$gate_number-$gate_head.out"
  set +e
  (
    cd "$gate_fixture/repo"
    PATH="$gate_fixture/bin:$PATH" \
      FAKE_GATE_MODE="$gate_mode" \
      FAKE_GATE_HEAD="$gate_head" \
      MANDATE_GATE_TIME="2026-08-04T12:00:00Z" \
      CLAUDE_CONFIG_DIR="$gate_fixture/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/gate.sh" \
        "placeholder-org/placeholder-repo#$gate_number"
  ) >"$gate_output_file"
  gate_status=$?
  set -e
  gate_output="$(cat "$gate_output_file")"
}

# All three verdicts retain distinct output and exit codes.
run_gate pass 7 aaaaaaaaaaaaaaaa
[ "$gate_status" -eq 0 ]
grep -q '^verdict: pass ' <<<"$gate_output"
grep -q '^condition required_checks: pass tier=content-derived ' <<<"$gate_output"
grep -q '^condition review_threads: pass tier=content-derived ' <<<"$gate_output"
grep -q '^condition mergeable: pass tier=content-derived detail={"mergeable":"MERGEABLE"}$' \
  <<<"$gate_output"
grep -q '^condition draft: pass tier=content-derived detail={"isDraft":false}$' \
  <<<"$gate_output"

# GitHub mergeability and draft state are unconditional gate mechanisms.
run_gate conflicting 13 1313131313131313
[ "$gate_status" -eq 1 ]
grep -q '^verdict: fail ' <<<"$gate_output"
grep -q '^condition mergeable: fail tier=content-derived detail={"mergeable":"CONFLICTING"}$' \
  <<<"$gate_output"

# A clean pull request still fails while its author has left it as a draft.
run_gate draft 14 1414141414141414
[ "$gate_status" -eq 1 ]
grep -q '^verdict: fail ' <<<"$gate_output"
grep -q '^condition required_checks: pass ' <<<"$gate_output"
grep -q '^condition mergeable: pass ' <<<"$gate_output"
grep -q '^condition draft: fail tier=content-derived detail={"isDraft":true}$' \
  <<<"$gate_output"

run_gate unknown-mergeable 15 1515151515151515
[ "$gate_status" -eq 2 ]
grep -q '^verdict: inconclusive ' <<<"$gate_output"
grep -q '^condition mergeable: inconclusive tier=content-derived detail={"mergeable":"UNKNOWN"}$' \
  <<<"$gate_output"

run_gate missing-mergeable 16 1616161616161616
[ "$gate_status" -eq 2 ]
grep -q '^verdict: inconclusive ' <<<"$gate_output"
grep -q '^condition mergeable: inconclusive tier=content-derived detail={"mergeable":null,' \
  <<<"$gate_output"

run_gate malformed-mergeable 17 1717171717171717
[ "$gate_status" -eq 2 ]
grep -q '^verdict: inconclusive ' <<<"$gate_output"
grep -q '^condition mergeable: inconclusive tier=content-derived detail={"mergeable":null,' \
  <<<"$gate_output"

run_gate tier 7 bbbbbbbbbbbbbbbb
[ "$gate_status" -eq 1 ]
grep -q '^verdict: fail ' <<<"$gate_output"
tier_line="$(grep '^condition bounce_selectors: fail ' <<<"$gate_output")"
grep -q 'tier=author-written,content-derived\|tier=content-derived,author-written' \
  <<<"$tier_line"
grep -q '"selector":"title:\*release\*","tier":"author-written"' \
  <<<"$tier_line"
grep -q '"selector":"path:.github/workflows/\*\*","tier":"content-derived"' \
  <<<"$tier_line"

run_gate unknown-check 7 cccccccccccccccc
[ "$gate_status" -eq 2 ]
grep -q '^verdict: inconclusive ' <<<"$gate_output"
grep -q '^condition required_checks: inconclusive tier=content-derived ' \
  <<<"$gate_output"

# A thread closed by the PR author remains unresolved to the gate under #18.
# This is the rule #55 must not weaken: replying is not resolving, and
# self-resolving is not either, so an author-resolved thread still fails —
# and, being resolved, contributes to neither the answered nor the unanswered
# count, both of which are 0 here alongside resolved_by_pr_author:1.
run_gate thread-author 7 dddddddddddddddd
[ "$gate_status" -eq 1 ]
grep -q '^condition review_threads: fail tier=content-derived ' <<<"$gate_output"
grep -q '"unresolved":0' <<<"$gate_output"
grep -q '"answered":0' <<<"$gate_output"
grep -q '"unanswered":0' <<<"$gate_output"
grep -q '"resolved_by_pr_author":1' <<<"$gate_output"

# A thread whose last comment is the reviewer's is unanswered: the author has
# not spoken since. It still fails review_threads, same as any open thread.
run_gate thread-unanswered 7 dededededededede
[ "$gate_status" -eq 1 ]
grep -q '^condition review_threads: fail tier=content-derived ' <<<"$gate_output"
grep -q '"unresolved":1' <<<"$gate_output"
grep -q '"answered":0' <<<"$gate_output"
grep -q '"unanswered":1' <<<"$gate_output"
grep -q '"resolved_by_pr_author":0' <<<"$gate_output"

# A thread whose last comment is the author's is answered, not resolved. #55:
# an answered thread still fails review_threads — the split adds information,
# it does not let a reply substitute for resolution.
run_gate thread-answered 7 efefefefefefefef
[ "$gate_status" -eq 1 ]
grep -q '^condition review_threads: fail tier=content-derived ' <<<"$gate_output"
grep -q '"unresolved":1' <<<"$gate_output"
grep -q '"answered":1' <<<"$gate_output"
grep -q '"unanswered":0' <<<"$gate_output"
grep -q '"resolved_by_pr_author":0' <<<"$gate_output"

# Already-judged state is keyed by (PR, head SHA): an unchanged re-read is
# marked, while a new commit on the same PR forces a fresh judgment.
run_gate pass 8 eeeeeeeeeeeeeeee
[ "$gate_status" -eq 0 ]
grep -q 'already_judged=false$' <<<"$(head -n 1 <<<"$gate_output")"
run_gate pass 8 eeeeeeeeeeeeeeee
[ "$gate_status" -eq 0 ]
grep -q 'already_judged=true$' <<<"$(head -n 1 <<<"$gate_output")"
run_gate pass 8 ffffffffffffffff
[ "$gate_status" -eq 0 ]
grep -q 'already_judged=false$' <<<"$(head -n 1 <<<"$gate_output")"

grant_gate_exception() {
  exception_condition="$1"
  exception_number="$2"
  exception_head="$3"
  shift 3
  PATH="$gate_fixture/bin:$PATH" \
    FAKE_GATE_HEAD="$exception_head" \
    MANDATE_EXCUSE_TIME="2026-08-04T11:30:00Z" \
    CLAUDE_CONFIG_DIR="$gate_fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/excuse.sh" grant \
      "placeholder-org/placeholder-repo#$exception_number" \
      "$exception_condition" "$@" >/dev/null
}

# A matching failed condition becomes excused and therefore aggregates to
# pass. Its reason remains visible in both output and the append-only trace.
excused_fail_head="2222222222222222222222222222222222222222"
grant_gate_exception bounce_selectors 9 "$excused_fail_head" \
  "principal accepted protected placeholder surface"
run_gate tier 9 "$excused_fail_head"
[ "$gate_status" -eq 0 ]
grep -q '^verdict: pass ' <<<"$gate_output"
excused_line="$(grep '^condition bounce_selectors: excused ' <<<"$gate_output")"
grep -q 'exception_reason="principal accepted protected placeholder surface"' \
  <<<"$excused_line"
! grep -q '^condition bounce_selectors: pass ' <<<"$gate_output"

# A same-SHA re-run is idempotent: already_judged changes only delivery state,
# while the exception and aggregate verdict remain the same.
run_gate tier 9 "$excused_fail_head"
[ "$gate_status" -eq 0 ]
grep -q '^verdict: pass ' <<<"$gate_output"
grep -q '^condition bounce_selectors: excused ' <<<"$gate_output"
grep -q 'already_judged=true$' <<<"$(head -n 1 <<<"$gate_output")"

# Inconclusive results are excusable by the same post-processing step.
excused_inconclusive_head="3333333333333333333333333333333333333333"
grant_gate_exception required_checks 10 "$excused_inconclusive_head" \
  "principal accepted unavailable placeholder check"
run_gate unknown-check 10 "$excused_inconclusive_head"
[ "$gate_status" -eq 0 ]
grep -q '^condition required_checks: excused ' <<<"$gate_output"
grep -q 'exception_reason="principal accepted unavailable placeholder check"' \
  <<<"$gate_output"

# A new head SHA silently invalidates the grant.
stale_exception_head="4444444444444444444444444444444444444444"
current_exception_head="5555555555555555555555555555555555555555"
grant_gate_exception bounce_selectors 11 "$stale_exception_head" \
  "principal accepted earlier placeholder artifact"
run_gate tier 11 "$current_exception_head"
[ "$gate_status" -eq 1 ]
grep -q '^condition bounce_selectors: fail ' <<<"$gate_output"
! grep -q 'principal accepted earlier placeholder artifact' <<<"$gate_output"

# A grant for a different condition cannot excuse the failing condition.
different_condition_head="6666666666666666666666666666666666666666"
grant_gate_exception reserved_refs 12 "$different_condition_head" \
  "principal accepted only placeholder reserved refs"
run_gate tier 12 "$different_condition_head"
[ "$gate_status" -eq 1 ]
grep -q '^condition bounce_selectors: fail ' <<<"$gate_output"
! grep -q '^condition bounce_selectors: excused ' <<<"$gate_output"

gate_log="$gate_fixture/config/ostrom/gate.jsonl"
[ "$(wc -l <"$gate_log" | tr -d '[:space:]')" -eq 19 ]
jq -s -e '
  ([.[] | select(.pr == "placeholder-org/placeholder-repo#8")]
    | map({head_sha, already_judged}))
  == [
    {head_sha: "eeeeeeeeeeeeeeee", already_judged: false},
    {head_sha: "eeeeeeeeeeeeeeee", already_judged: true},
    {head_sha: "ffffffffffffffff", already_judged: false}
  ]
  and all(.[];
    (.conditions | length) == 6
    and all(.conditions[];
      has("result") and has("tier") and (.tier | type == "array")
    )
  )
' "$gate_log" >/dev/null
jq -s -e --arg head "$excused_fail_head" '
  ([.[] | select(
    .pr == "placeholder-org/placeholder-repo#9" and .head_sha == $head
  )]) as $runs
  | ($runs | length) == 2
  and all($runs[];
    .verdict == "pass"
    and (([.conditions[] | select(.name == "bounce_selectors")][0]) as $condition
      | $condition.result == "excused"
      and $condition.exception_reason
        == "principal accepted protected placeholder surface"
      and $condition.result != "pass")
  )
' "$gate_log" >/dev/null

# The publisher's fixtures are synthetic and its only push target is a local
# throwaway bare repository. Dry-run exits before any clone or remote access.
publisher="$fixture/publisher"
publisher_config="$publisher/config"
publisher_data="$publisher_config/ostrom"
publisher_remote="$publisher/state.git"
publisher_cache="$publisher/cache"
mkdir -p "$publisher_data"
cat >"$publisher_data/mandates.yaml" <<'YAML'
provider: file
cadence_hours: 24
stuck_after_days: 7
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated: []
    excluded: []
    reserved: []
    default: unclassified
    paused: false
    bounce: []
YAML
cat >"$publisher_data/queue.jsonl" <<'JSONL'
{"id":"example-org/example-repo#101","repo":"example-org/example-repo","ref":"#101","title":"chore(example): synthetic queue item","kind":"decision","mandate":{"reason":"synthetic policy reason","dossier":{"question":"synthetic question","recommended_action":"approve","blast_radius":"one synthetic file","options_ruled_out":["defer"]}},"state":"pending","opened":"2026-07-31T09:00:00Z","age_days":1,"aged_out":false,"needs_judgment":true,"blocked_by":[],"unlisted_queue_field":"drop-me"}
JSONL
cat >"$publisher_data/gate.jsonl" <<'JSONL'
{"ts":"2026-04-01T12:00:00Z","pr":"example-org/example-repo#200","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","verdict":"fail","already_judged":false,"conditions":[{"name":"mergeable","result":"fail","tier":["content-derived"],"detail":{"mergeable":"CONFLICTING","reason":"synthetic reason"}}]}
{"ts":"2026-07-31T23:59:59Z","pr":"example-org/example-repo#201","head_sha":null,"verdict":"inconclusive","already_judged":false,"conditions":[{"name":"required_checks","result":"inconclusive","tier":["content-derived"],"detail":{"selectors":[{"selector":"verify-*","result":"pass","matches":[{"name":"verify-linux","state":"SUCCESS"}]}],"reason":"synthetic reason"}}]}
{"ts":"2026-08-01T00:00:01Z","pr":"example-org/example-repo#202","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","verdict":"pass","already_judged":false,"conditions":[{"name":"mergeable","result":"pass","tier":["content-derived"],"detail":{"mergeable":"CONFLICTING"}},{"name":"reserved_refs","result":"excused","tier":["author-written"],"detail":{"matches":["ref:#101"],"reason":"synthetic reason"},"exception_reason":"synthetic exception"},{"name":"bounce_selectors","result":"pass","tier":["author-written","content-derived"],"detail":{"matches":[{"selector":"title:*synthetic*","tier":"author-written"}],"unobservable":[{"selector":"path:synthetic/**","tier":"content-derived","error":"synthetic raw error"}],"reason":"synthetic reason"}},{"name":"future_condition","result":"pass","tier":[],"detail":{"future":"synthetic value"}}]}
JSONL
cat >"$publisher_data/state.json" <<'JSON'
{
  "version": 2,
  "sweep_mode": "incremental",
  "last_full_reconciliation": "2026-08-01T00:00:00Z",
  "dead_selectors": [
    {"repo": "example-org/example-repo", "selector": "label:synthetic", "source": "delegated"}
  ],
  "repos": {
    "example-org/example-repo": {
      "cursor": "cursor:synthetic-2",
      "previous_cursor": "cursor:synthetic-1",
      "selector_hash": "synthetic-selector-hash",
      "items": {
        "example-org/example-repo#101": {
          "classification": "unclassified",
          "fingerprint": "synthetic-fingerprint",
          "first_seen": "2026-07-31T09:00:00Z",
          "updated": "2026-08-01T00:00:00Z",
          "matched_selector": "default:unclassified",
          "stuck": false
        }
      },
      "policy": {
        "default": "unclassified",
        "delegated": [],
        "reserved": [],
        "bounce": [],
        "bounce_all": [],
        "excluded": [],
        "paused": false,
        "selector_hash": "synthetic-selector-hash"
      },
      "scope_changes": {"entered": [], "left": []},
      "unclassified": 1,
      "item_cap": null,
      "notice": {"kind": "baseline", "reported": false, "text": "synthetic explanatory notice"}
    }
  }
}
JSON

# A scratch config with the implicit destination is refused before any GitHub
# or Git command can face the real hub. The dedicated status lets sweep.sh
# report this as a deliberate skip rather than success or infrastructure
# failure. These spies must remain untouched.
publish_spy_bin="$publisher/spy-bin"
publish_spy_log="$publisher/destination-command.log"
mkdir -p "$publish_spy_bin"
for publish_spy_command in gh git; do
  cat >"$publish_spy_bin/$publish_spy_command" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$OSTROM_TEST_PUBLISH_SPY_LOG"
exit 97
SH
  chmod +x "$publish_spy_bin/$publish_spy_command"
done
set +e
PATH="$publish_spy_bin:$PATH" \
  OSTROM_TEST_PUBLISH_SPY_LOG="$publish_spy_log" \
  CLAUDE_CONFIG_DIR="$publisher_config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
  bash "$PLUGIN_ROOT/scripts/publish.sh" \
  >"$publisher/refused.out" 2>"$publisher/refused.err"
publish_refused_status=$?
set -e
if [ "$publish_refused_status" -ne 3 ]; then
  echo "scratch publish must return the deliberate-skip status" >&2
  exit 1
fi
if [ "$(grep -Fc "$publisher_config" "$publisher/refused.err")" -eq 0 ]; then
  echo "scratch publish refusal must name its config directory" >&2
  exit 1
fi
if [ "$(grep -Fc 'onsager-ai/ostrom-hub' "$publisher/refused.err")" -eq 0 ]; then
  echo "scratch publish refusal must name the refused destination" >&2
  exit 1
fi
if [ -s "$publisher/refused.out" ]; then
  echo "scratch publish refusal must not report successful output" >&2
  exit 1
fi
if [ -e "$publish_spy_log" ]; then
  echo "scratch publish must not attempt a destination-facing command" >&2
  exit 1
fi
if [ "$(grep -Fc 'destination follows the config the sweep read' \
  "$PLUGIN_ROOT/scripts/publish.sh")" -eq 0 ]; then
  echo "publish header must explain that destination follows sweep config" >&2
  exit 1
fi

run_publish_dry() {
  CLAUDE_CONFIG_DIR="$publisher_config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_PUBLISH_REMOTE="$publisher_remote" \
    MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
    bash "$PLUGIN_ROOT/scripts/publish.sh" --dry-run
}

publisher_tree="$publisher/tree.json"
run_publish_dry >"$publisher_tree"
allowlist="$PLUGIN_ROOT/config/publish-allowlist.json"
assert_tree_allowlisted() {
  candidate_tree="$1"
  jq -e --slurpfile allow_file "$allowlist" '
    $allow_file[0] as $allow
    | . as $tree
    | all($tree["queue.jsonl"][];
        ((keys - $allow.queue) | length) == 0
        and ((.mandate | keys) - $allow["queue.mandate"] | length) == 0
        and (if (.mandate.dossier | type) == "object"
          then ((.mandate.dossier | keys) - $allow["queue.mandate.dossier"] | length) == 0
          else true end)
      )
    and all(
      $tree | to_entries[] | select(.key | startswith("gate/")) | .value[];
      ((keys - $allow.gate) | length) == 0
      and all(.conditions[];
        ((keys - $allow["gate.condition"]) | length) == 0
        and (if has("detail") then
          (($allow["gate.detail." + .name] // null) != null)
          and (((.detail | keys) - $allow["gate.detail." + .name] | length) == 0)
          else true end)
        and (if .name == "bounce_selectors" and has("detail") then
          all(.detail.matches[];
            ((keys - $allow["gate.detail.bounce_selectors.match"]) | length) == 0)
          and all(.detail.unobservable[];
            ((keys - $allow["gate.detail.bounce_selectors.unobservable"]) | length) == 0)
          else true end)
        and (if .name == "required_checks" and has("detail") then
          all(.detail.selectors[];
            ((keys - $allow["gate.detail.required_checks.selector"]) | length) == 0
            and all(.matches[];
              ((keys - $allow["gate.detail.required_checks.match"]) | length) == 0)
          )
          else true end)
      )
    )
    and ((($tree["state.json"] | keys) - $allow.state | length) == 0)
    and all($tree["state.json"].dead_selectors[];
      ((keys - $allow["state.dead_selector"]) | length) == 0
    )
    and all($tree["state.json"].repos | to_entries[].value;
      ((keys - $allow["state.repo"]) | length) == 0
      and all(.items | to_entries[].value;
        ((keys - $allow["state.item"]) | length) == 0
      )
      and (((.policy | keys) - $allow["state.policy"] | length) == 0)
      and (((.scope_changes | keys) - $allow["state.scope_changes"] | length) == 0)
      and (if (.notice | type) == "object"
        then (((.notice | keys) - $allow["state.notice"] | length) == 0)
        else true end)
    )
  ' "$candidate_tree" >/dev/null
}
assert_tree_allowlisted "$publisher_tree"
jq -e '
  .["state.json"].sweep_mode == "incremental"
  and .["state.json"].last_full_reconciliation == "2026-08-01T00:00:00Z"
' "$publisher_tree" >/dev/null

# A workstation with live records exercises the same assertion without
# making it a CI prerequisite. Dry-run never enters clone/bootstrap logic.
real_config_dir="${OSTROM_TEST_REAL_CONFIG_DIR:-$HOME/.claude}"
if [ -f "$real_config_dir/ostrom/mandates.yaml" ] && \
  [ -f "$real_config_dir/ostrom/queue.jsonl" ] && \
  [ -f "$real_config_dir/ostrom/gate.jsonl" ] && \
  [ -f "$real_config_dir/ostrom/state.json" ]; then
  CLAUDE_CONFIG_DIR="$real_config_dir" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/publish.sh" --dry-run \
    >"$publisher/live-tree.json"
  assert_tree_allowlisted "$publisher/live-tree.json"
fi
jq -e '
  .["queue.jsonl"][0] | has("unlisted_queue_field") | not
' "$publisher_tree" >/dev/null
jq -e '
  .["manifest.json"].dropped_fields.queue.unlisted_queue_field == 1
  and .["manifest.json"].dropped_fields.state["repos.*.notice.text"] == 1
' "$publisher_tree" >/dev/null
# Unknown condition names fail closed: their envelope remains, their entire
# detail disappears, and the omission is counted.
jq -e '
  first(
    .["gate/2026-08-01.jsonl"][].conditions[]
    | select(.name == "future_condition")
  ) as $condition
  | ($condition | has("detail") | not)
  and .["manifest.json"].dropped_fields.gate["conditions[].detail"] == 1
' "$publisher_tree" >/dev/null
# The two observed free-text carriers are excluded while adjacent factual
# detail survives, and both dropped paths are visible in the manifest.
jq -e '
  .["manifest.json"].dropped_fields.gate["conditions[].detail.reason"] == 4
  and .["manifest.json"].dropped_fields.gate[
    "conditions[].detail.unobservable[].error"
  ] == 1
  and first(
    .["gate/2026-08-01.jsonl"][].conditions[]
    | select(.name == "bounce_selectors")
  ).detail.unobservable == [
    {selector: "path:synthetic/**", tier: "content-derived"}
  ]
  and ([
    . | to_entries[] | select(.key | startswith("gate/")) | .value[]
    | .conditions[] | .detail? | .. | objects | keys[]
    | select(. == "reason" or . == "error")
  ] | length) == 0
' "$publisher_tree" >/dev/null
jq -e '
  has("gate/2026-07-31.jsonl")
  and (has("gate/2026-04-01.jsonl") | not)
  and .["gate/2026-07-31.jsonl"][0].ts == "2026-07-31T23:59:59Z"
  and has("gate/2026-08-01.jsonl")
  and .["rollup.json"].verdicts_by_day["2026-04-01"].fail == 1
  and .["rollup.json"].verdicts_by_day["2026-07-31"].inconclusive == 1
  and .["rollup.json"].queue_age_buckets["0-1"] == 1
  and .["rollup.json"].repo_classifications["example-org/example-repo"].unclassified == 1
  and first(
    .["gate/2026-08-01.jsonl"][].conditions[]
    | select(.name == "mergeable")
  ).detail.mergeable == "CONFLICTING"
  and first(
    .["gate/2026-07-31.jsonl"][].conditions[]
    | select(.name == "required_checks")
  ).detail.selectors[0].matches[0] == {name: "verify-linux", state: "SUCCESS"}
' "$publisher_tree" >/dev/null

# Backfill is a pure derivation: a second run at the same publication instant
# produces identical daily files and an identical full tree.
run_publish_dry >"$publisher/tree-again.json"
cmp -s "$publisher_tree" "$publisher/tree-again.json"

changed_allowlist="$publisher/changed-allowlist.json"
jq '.queue += ["fixture_extension"]' "$allowlist" >"$changed_allowlist"
CLAUDE_CONFIG_DIR="$publisher_config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  MANDATE_PUBLISH_ALLOWLIST="$changed_allowlist" \
  MANDATE_PUBLISH_REMOTE="$publisher_remote" \
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
  bash "$PLUGIN_ROOT/scripts/publish.sh" --dry-run \
  >"$publisher/changed-tree.json"
[ "$(jq -r '.["manifest.json"].schema_id' "$publisher_tree")" != \
  "$(jq -r '.["manifest.json"].schema_id' "$publisher/changed-tree.json")" ]

# A real commit and push round-trip goes only to this local bare repository.
git init --bare --quiet "$publisher_remote"
publish_env=(
  CLAUDE_CONFIG_DIR="$publisher_config"
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT"
  MANDATE_PUBLISH_DIR="$publisher_cache"
  MANDATE_PUBLISH_REMOTE="$publisher_remote"
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z"
  GIT_AUTHOR_NAME="Ostrom Test"
  GIT_AUTHOR_EMAIL="ostrom@example.test"
  GIT_COMMITTER_NAME="Ostrom Test"
  GIT_COMMITTER_EMAIL="ostrom@example.test"
)
env "${publish_env[@]}" bash "$PLUGIN_ROOT/scripts/publish.sh" >/dev/null
[ "$(git --git-dir="$publisher_remote" rev-list --count state)" -eq 1 ]
git --git-dir="$publisher_remote" show state:gate/2026-07-31.jsonl |
  jq -e '.ts == "2026-07-31T23:59:59Z"' >/dev/null
env "${publish_env[@]}" bash "$PLUGIN_ROOT/scripts/publish.sh" >/dev/null
[ "$(git --git-dir="$publisher_remote" rev-list --count state)" -eq 1 ]

# An explicit non-default destination is the caller's scratch-to-scratch
# declaration and must bypass only the implicit-hub guard.
override_remote="$publisher/explicit-override.git"
override_cache="$publisher/explicit-override-cache"
git init --bare --quiet "$override_remote"
set +e
CLAUDE_CONFIG_DIR="$publisher_config" \
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  MANDATE_PUBLISH_DIR="$override_cache" \
  MANDATE_PUBLISH_REMOTE="$override_remote" \
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
  GIT_AUTHOR_NAME="Ostrom Test" \
  GIT_AUTHOR_EMAIL="ostrom@example.test" \
  GIT_COMMITTER_NAME="Ostrom Test" \
  GIT_COMMITTER_EMAIL="ostrom@example.test" \
  bash "$PLUGIN_ROOT/scripts/publish.sh" \
  >"$publisher/explicit-override.out" 2>"$publisher/explicit-override.err"
explicit_override_status=$?
set -e
if [ "$explicit_override_status" -ne 0 ]; then
  echo "scratch publish must honor an explicit non-default destination" >&2
  exit 1
fi
explicit_override_count="$(
  git --git-dir="$override_remote" rev-list --count state 2>/dev/null || printf '0\n'
)"
if [ "$explicit_override_count" -ne 1 ]; then
  echo "explicit scratch destination must receive one state publication" >&2
  exit 1
fi

# Both spellings of the operator config retain the implicit hub destination:
# unset CLAUDE_CONFIG_DIR and an explicit $HOME/.claude. A purpose-built gh
# adapter maps that exact owner/repo argument to a local bare repository, so
# the test exercises clone, state-branch publication, and content without
# network access.
operator_home="$publisher/operator-home"
operator_config="$operator_home/.claude"
operator_remote="$publisher/operator-default.git"
operator_bin="$publisher/operator-bin"
operator_clone_log="$publisher/operator-clone.log"
mkdir -p "$operator_config" "$operator_bin"
cp -R "$publisher_data" "$operator_config/ostrom"
git init --bare --quiet "$operator_remote"
cat >"$operator_bin/gh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -ne 6 ] || [ "$1" != repo ] || [ "$2" != clone ] || \
  [ "$3" != onsager-ai/ostrom-hub ] || [ "$5" != -- ] || \
  [ "$6" != --no-checkout ]; then
  exit 98
fi
printf '%s\n' "$3" >>"$OSTROM_TEST_OPERATOR_CLONE_LOG"
exec "$OSTROM_TEST_REAL_GIT" clone --no-checkout \
  "$OSTROM_TEST_OPERATOR_REMOTE" "$4"
SH
chmod +x "$operator_bin/gh"
operator_publish_env=(
  HOME="$operator_home"
  PATH="$operator_bin:$PATH"
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT"
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z"
  OSTROM_TEST_OPERATOR_CLONE_LOG="$operator_clone_log"
  OSTROM_TEST_OPERATOR_REMOTE="$operator_remote"
  OSTROM_TEST_REAL_GIT="$(command -v git)"
  GIT_AUTHOR_NAME="Ostrom Test"
  GIT_AUTHOR_EMAIL="ostrom@example.test"
  GIT_COMMITTER_NAME="Ostrom Test"
  GIT_COMMITTER_EMAIL="ostrom@example.test"
)
set +e
(
  unset CLAUDE_CONFIG_DIR
  env "${operator_publish_env[@]}" \
    MANDATE_PUBLISH_DIR="$publisher/operator-unset-cache" \
    bash "$PLUGIN_ROOT/scripts/publish.sh"
) >"$publisher/operator-unset.out" 2>"$publisher/operator-unset.err"
operator_unset_status=$?
set -e
if [ "$operator_unset_status" -ne 0 ]; then
  echo "unset CLAUDE_CONFIG_DIR must retain operator publication" >&2
  exit 1
fi
set +e
env "${operator_publish_env[@]}" \
  CLAUDE_CONFIG_DIR="$operator_config" \
  MANDATE_PUBLISH_DIR="$publisher/operator-explicit-cache" \
  bash "$PLUGIN_ROOT/scripts/publish.sh" \
  >"$publisher/operator-explicit.out" 2>"$publisher/operator-explicit.err"
operator_explicit_status=$?
set -e
if [ "$operator_explicit_status" -ne 0 ]; then
  echo "explicit default CLAUDE_CONFIG_DIR must retain operator publication" >&2
  exit 1
fi
if [ "$(grep -Fc 'onsager-ai/ostrom-hub' "$operator_clone_log")" -ne 2 ]; then
  echo "both operator config forms must use the unchanged default destination" >&2
  exit 1
fi
operator_publish_count="$(git --git-dir="$operator_remote" rev-list --count state)"
if [ "$operator_publish_count" -ne 1 ]; then
  echo "operator config must publish the unchanged snapshot to state" >&2
  exit 1
fi
operator_tree_id="$(git --git-dir="$operator_remote" rev-parse 'state^{tree}')"
explicit_tree_id="$(git --git-dir="$publisher_remote" rev-parse 'state^{tree}')"
if [ "$operator_tree_id" != "$explicit_tree_id" ]; then
  echo "operator config publication content must remain unchanged" >&2
  exit 1
fi

# A `--no-checkout` clone still populates the index from the remote's
# default branch even though it skips the working tree, so a remote whose
# default branch already carries a file (and has no `state` branch at all)
# is the case that actually exercises the inheritance: `checkout --orphan`
# would otherwise carry that file's index entry straight into the first
# publish commit. Assert the resulting tree holds exactly the derived
# paths and nothing else.
orphan_remote="$publisher/orphan-remote.git"
orphan_seed="$publisher/orphan-seed"
git init --bare --quiet "$orphan_remote"
git clone --quiet "$orphan_remote" "$orphan_seed"
git -C "$orphan_seed" config user.name "Ostrom Test"
git -C "$orphan_seed" config user.email "ostrom@example.test"
git -C "$orphan_seed" checkout -b main --quiet
printf 'unrelated default-branch content\n' >"$orphan_seed/README.md"
git -C "$orphan_seed" add README.md
git -C "$orphan_seed" commit --quiet -m "seed unrelated default branch"
git -C "$orphan_seed" push --quiet origin HEAD:main
git --git-dir="$orphan_remote" symbolic-ref HEAD refs/heads/main
orphan_cache="$publisher/orphan-cache"
orphan_env=(
  CLAUDE_CONFIG_DIR="$publisher_config"
  CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT"
  MANDATE_PUBLISH_DIR="$orphan_cache"
  MANDATE_PUBLISH_REMOTE="$orphan_remote"
  MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z"
  GIT_AUTHOR_NAME="Ostrom Test"
  GIT_AUTHOR_EMAIL="ostrom@example.test"
  GIT_COMMITTER_NAME="Ostrom Test"
  GIT_COMMITTER_EMAIL="ostrom@example.test"
)
env "${orphan_env[@]}" bash "$PLUGIN_ROOT/scripts/publish.sh" >/dev/null
[ "$(git --git-dir="$orphan_remote" rev-list --count state)" -eq 1 ]
orphan_tree="$(git --git-dir="$orphan_remote" ls-tree -r --name-only state | sort)"
expected_orphan_tree="$(printf '%s\n' \
  gate/2026-07-31.jsonl gate/2026-08-01.jsonl manifest.json queue.jsonl \
  rollup.json state.json | sort)"
[ "$orphan_tree" = "$expected_orphan_tree" ]

# A bare repository whose hook rejects the push cannot fail the governing
# sweep or mutate any source record. The first sweep establishes a stable
# baseline; only the second run is forced to fail publication.
publish_sweep="$publisher/sweep-failure"
rejecting_remote="$publish_sweep/rejecting.git"
mkdir -p "$publish_sweep/config/ostrom" "$publish_sweep/repo"
write_gatekeeper_secrets "$publish_sweep/config"
cp "$publisher_data/mandates.yaml" "$publish_sweep/config/ostrom/mandates.yaml"

# The sweep translates the publisher's config-guard status into a deliberate
# skip. The gh call log proves that removing the guard would attempt the real
# owner/repo destination, while the guarded run never reaches repo clone.
set +e
(
  cd "$publish_sweep/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$publish_sweep/guard-gh.log" \
    CLAUDE_CONFIG_DIR="$publish_sweep/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SWEEP_MODE=full \
    MANDATE_PUBLISH_DIR="$publish_sweep/guard-cache" \
    MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
    GIT_AUTHOR_NAME="Ostrom Test" \
    GIT_AUTHOR_EMAIL="ostrom@example.test" \
    GIT_COMMITTER_NAME="Ostrom Test" \
    GIT_COMMITTER_EMAIL="ostrom@example.test" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" \
    >"$publish_sweep/guard.out" 2>"$publish_sweep/guard.err"
)
guard_sweep_status=$?
set -e
if [ "$guard_sweep_status" -ne 0 ]; then
  echo "scratch publish guard must not fail the governing sweep" >&2
  exit 1
fi
if [ "$(grep -Fc "$publish_sweep/config" "$publish_sweep/guard.err")" -eq 0 ]; then
  echo "sweep publish refusal must name the scratch config directory" >&2
  exit 1
fi
if [ "$(grep -Fc 'onsager-ai/ostrom-hub' "$publish_sweep/guard.err")" -eq 0 ]; then
  echo "sweep publish refusal must name the refused destination" >&2
  exit 1
fi
if [ "$(grep -Fc 'mandate sweep: publish deliberately skipped by config guard' \
  "$publish_sweep/guard.err")" -eq 0 ]; then
  echo "sweep must report a guarded publication as a deliberate skip" >&2
  exit 1
fi
if grep -Fq 'mandate sweep: publish failed' "$publish_sweep/guard.err"; then
  echo "config-guard skip must remain distinct from publish failure" >&2
  exit 1
fi
if grep -Fq 'mandate publish: published' "$publish_sweep/guard.out"; then
  echo "config-guard skip must remain distinct from publish success" >&2
  exit 1
fi
if grep -Fq $'\trepo clone onsager-ai/ostrom-hub ' \
  "$publish_sweep/guard-gh.log"; then
  echo "scratch sweep must not attempt to clone the real hub" >&2
  exit 1
fi

(
  cd "$publish_sweep/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$publish_sweep/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SWEEP_MODE=full \
    MANDATE_PUBLISH_DIR="$publish_sweep/first-cache" \
    MANDATE_PUBLISH_REMOTE="$publisher_remote" \
    MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
    GIT_AUTHOR_NAME="Ostrom Test" \
    GIT_AUTHOR_EMAIL="ostrom@example.test" \
    GIT_COMMITTER_NAME="Ostrom Test" \
    GIT_COMMITTER_EMAIL="ostrom@example.test" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
source_hashes_before="$(
  sha256sum "$publish_sweep/config/ostrom/queue.jsonl" \
    "$publish_sweep/config/ostrom/state.json"
)"
git init --bare --quiet "$rejecting_remote"
git --git-dir="$rejecting_remote" config core.hooksPath "$rejecting_remote/hooks"
cat >"$rejecting_remote/hooks/pre-receive" <<'SH'
#!/bin/sh
echo "synthetic push rejection" >&2
exit 1
SH
chmod +x "$rejecting_remote/hooks/pre-receive"
set +e
(
  cd "$publish_sweep/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$publish_sweep/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SWEEP_MODE=full \
    MANDATE_PUBLISH_DIR="$publish_sweep/failing-cache" \
    MANDATE_PUBLISH_REMOTE="$rejecting_remote" \
    MANDATE_PUBLISH_TIME="2026-08-01T00:05:00Z" \
    GIT_AUTHOR_NAME="Ostrom Test" \
    GIT_AUTHOR_EMAIL="ostrom@example.test" \
    GIT_COMMITTER_NAME="Ostrom Test" \
    GIT_COMMITTER_EMAIL="ostrom@example.test" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null 2>"$publish_sweep/failure.err"
)
publish_sweep_status=$?
set -e
[ "$publish_sweep_status" -eq 0 ]
grep -q 'mandate sweep: publish failed; local records remain authoritative' \
  "$publish_sweep/failure.err"
grep -q 'synthetic push rejection' "$publish_sweep/failure.err"
source_hashes_after="$(
  sha256sum "$publish_sweep/config/ostrom/queue.jsonl" \
    "$publish_sweep/config/ostrom/state.json"
)"
[ "$source_hashes_before" = "$source_hashes_after" ]
[ ! -e "$publish_sweep/config/ostrom/gate.jsonl" ]

echo "mandate tests: ok"
