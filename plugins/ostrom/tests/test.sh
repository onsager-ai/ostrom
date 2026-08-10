#!/usr/bin/env bash

set -euo pipefail

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

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
work_frontmatter="$(
  awk 'NR == 1 { next } /^---$/ { exit } { print }' "$work_skill"
)"
grep -q 'lease.sh\" acquire "\$lease_owner"' "$gatekeep_skill"
grep -q 'lease.sh\" release "\$lease_owner"' "$gatekeep_skill"
grep -q 'Never infer concurrency or lease' "$gatekeep_skill"
for trace_kind in pass-started item-selected pass-ended; do
  grep -q "trace.sh\" append $trace_kind" "$gatekeep_skill"
done
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

CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$fake_marker" ]
[ ! -e "$pass_config/ostrom/sprint.jsonl" ]

: >"$pass_config/ostrom/loop-armed"
CLAUDE_CONFIG_DIR="$pass_config" MANDATE_LEASE_NOW_EPOCH=400 \
  MANDATE_LEASE_NAME=builder-pass.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" acquire fixture-holder 60 >/dev/null
CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null 2>&1
[ ! -e "$fake_marker" ]
[ ! -e "$pass_config/ostrom/sprint.jsonl" ]
CLAUDE_CONFIG_DIR="$pass_config" MANDATE_LEASE_NAME=builder-pass.lease \
  bash "$PLUGIN_ROOT/scripts/lease.sh" release fixture-holder

CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
[ ! -e "$pass_config/ostrom/builder-pass.lease" ]
jq -s -e '
  length == 2
  and map(.kind) == ["pass-started", "pass-ended"]
  and (.[0].fact.owner | test("^builder-[0-9a-f]{8}-wake1$"))
  and .[1].fact.owner == .[0].fact.owner
  and .[1].fact.outcome == "completed"
  and .[1].fact.cost_usd == 1.25
  and (.[1].fact.duration_seconds | type == "number" and . >= 0)
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

CLAUDE_CONFIG_DIR="$pass_config" CLAUDE_BIN="$fake_claude" \
  FAKE_CLAUDE_MARKER="$fake_marker" \
  bash "$PLUGIN_ROOT/scripts/pass.sh" builder >/dev/null
jq -s -e '
  length == 4
  and .[2].kind == "pass-started"
  and .[3].kind == "pass-ended"
  and (.[2].fact.owner | test("^builder-[0-9a-f]{8}-wake2$"))
  and (.[0].fact.owner | split("-wake")[0])
    == (.[2].fact.owner | split("-wake")[0])
  and .[3].fact.owner == .[2].fact.owner
' "$pass_config/ostrom/sprint.jsonl" >/dev/null

FAKE_CLAUDE_MODE=wait FAKE_CLAUDE_MARKER="$fake_marker" \
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
grep -Fxq 'app-token: builder credentials are not configured' \
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
exit 1
EOF
chmod +x "$fixture/bin/gh"

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
      CLAUDE_CONFIG_DIR="$fixture/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}

# Issue bodies can exceed Linux's per-argument limit. The sweep must extract
# dependency refs without ever carrying the raw body through jq argv.
large_body="$fixture/large-body"
mkdir -p "$large_body/config/ostrom" "$large_body/repo"
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

# Hitting either GitHub query limit is durable sweep state, not a silent
# partial portfolio. The digest keeps warning until a later sweep is below it.
capped="$fixture/capped"
mkdir -p "$capped/config/ostrom" "$capped/repo"
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
(
  cd "$capped/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$capped/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null
)
jq -e '
  .repos["example-org/capped"].item_cap == 200
' "$capped/config/ostrom/state.json" >/dev/null
capped_digest="$(
  cd "$capped/repo"
  CLAUDE_CONFIG_DIR="$capped/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
capped_digest_text="$(jq -r '.systemMessage' <<<"$capped_digest")"
grep -q \
  '^example-org/capped: item cap reached (200) — sweep may be incomplete$' \
  <<<"$capped_digest_text"
[ "$(
  grep -c '^example-org/capped: item cap reached' <<<"$capped_digest_text"
)" -eq 1 ]
capped_digest_again="$(
  cd "$capped/repo"
  CLAUDE_CONFIG_DIR="$capped/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
capped_digest_again_text="$(jq -r '.systemMessage' <<<"$capped_digest_again")"
grep -q \
  '^example-org/capped: item cap reached (200) — sweep may be incomplete$' \
  <<<"$capped_digest_again_text"

# A representative eight-project first sweep remains a compact digest.
portfolio="$fixture/portfolio"
mkdir -p "$portfolio/config/ostrom" "$portfolio/repo"
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
[ "$(wc -l <<<"$stale_digest_text")" -eq 2 ]
grep -q '^STALE — mandate sweep overdue$' <<<"$stale_digest_text"
grep -q '^2 projects nominal$' <<<"$stale_digest_text"

# Unclassified items are event-gated, not safety carve-outs: a dormant
# unclassified item stays invisible sweep after sweep, but the moment it
# moves (a title/fingerprint change) it surfaces as a decision — the
# invisibility the audit found is fixed without dumping the whole dormant
# backlog into one digest.
uncat="$fixture/uncat"
mkdir -p "$uncat/config/ostrom" "$uncat/repo"
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
grep -q 'gh is not authenticated' <<<"$auth_message"

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
{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":[{"id":"THREAD_placeholder","isResolved":true,"resolvedBy":{"login":"builder-login"}}],"pageInfo":{"hasNextPage":false,"endCursor":"cursor-placeholder"}}}}}}
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
run_gate thread-author 7 dddddddddddddddd
[ "$gate_status" -eq 1 ]
grep -q '^condition review_threads: fail tier=content-derived ' <<<"$gate_output"
grep -q '"resolved_by_pr_author":1' <<<"$gate_output"

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
[ "$(wc -l <"$gate_log" | tr -d '[:space:]')" -eq 17 ]
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

run_publish_dry() {
  CLAUDE_CONFIG_DIR="$publisher_config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
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
cp "$publisher_data/mandates.yaml" "$publish_sweep/config/ostrom/mandates.yaml"
(
  cd "$publish_sweep/repo"
  PATH="$fixture/bin:$PATH" \
    CLAUDE_CONFIG_DIR="$publish_sweep/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
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
