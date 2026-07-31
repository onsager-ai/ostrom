#!/usr/bin/env bash

set -euo pipefail

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/config/ostrom" "$fixture/repo" "$fixture/bin"

cp "$PLUGIN_ROOT/config/mandates.example.yaml" "$fixture/config/ostrom/mandates.yaml"

# Prove shipped < user < repo precedence without involving a YAML library.
mkdir -p "$fixture/layers/config/ostrom" "$fixture/layers/repo/.ostrom"
cat >"$fixture/layers/config/ostrom/mandates.yaml" <<'YAML'
cadence_hours: 12
stuck_after_days: 5
bounce_all:
  - user boundary
projects:
  - repo: example-org/example-repo
    delegated: user delegated outcome
    bounce:
      - user project boundary
YAML
cat >"$fixture/layers/repo/.ostrom/mandates.yaml" <<'YAML'
stuck_after_days: 2
bounce_all:
  - repo boundary
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
  and .bounce_all == ["repo boundary"]
  and .projects[0].repo == "example-org/example-repo"
  and .projects[0].delegated == "user delegated outcome"
  and .projects[0].paused == false
' <<<"$layered" >/dev/null

# A project without the authorization half of its mandate fails clearly.
mkdir -p "$fixture/missing-delegated/config/ostrom" "$fixture/missing-delegated/repo"
cat >"$fixture/missing-delegated/config/ostrom/mandates.yaml" <<'YAML'
projects:
  - repo: example-org/example-repo
    bounce: []
YAML
set +e
delegated_message="$(
  cd "$fixture/missing-delegated/repo"
  CLAUDE_CONFIG_DIR="$fixture/missing-delegated/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash -c 'source "$CLAUDE_PLUGIN_ROOT/scripts/mandate-lib.sh"; mandate_load_config' 2>&1
)"
delegated_status=$?
set -e
[ "$delegated_status" -eq 2 ]
grep -q 'example-org/example-repo is missing required delegated outcome' <<<"$delegated_message"

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
  printf '%s\t%s %s\n' "$repo" "$1" "$2" >>"$FAKE_GH_CALL_LOG"
fi

if [ "$1 $2" = "auth status" ]; then
  [ "${FAKE_GH_AUTH_FAIL:-0}" != "1" ]
  exit 0
fi
if [ "$1 $2" = "issue list" ]; then
  if [ "$repo" = "example-org/another-repo" ]; then
    echo "paused project issue query" >&2
    exit 90
  fi
  cat <<'JSON'
[{"number":7,"title":"Plan a production deployment","labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/7"},{"number":9,"title":"Launch a marketing campaign","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/9"}]
JSON
  exit 0
fi
if [ "$1 $2" = "pr list" ]; then
  if [ "$repo" = "example-org/another-repo" ]; then
    cat <<'JSON'
[{"number":20,"title":"Plan a production deployment","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/20","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"FAILURE","status":"COMPLETED"}]}]
JSON
  else
    cat <<'JSON'
[{"number":8,"title":"Routine maintenance","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/8","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}]}]
JSON
  fi
  exit 0
fi
exit 1
EOF
chmod +x "$fixture/bin/gh"

run_sweep() {
  (
    cd "$fixture/repo"
    PATH="$fixture/bin:$PATH" \
      FAKE_GH_CALL_LOG="$fixture/gh-calls" \
      CLAUDE_CONFIG_DIR="$fixture/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}

run_sweep >/dev/null
queue="$fixture/config/ostrom/queue.jsonl"
state="$fixture/config/ostrom/state.json"

jq -e 'select(.id == "example-org/example-repo#7" and .kind == "tripwire")
  | .mandate.dossier
  | has("question") and has("options_ruled_out")
    and has("recommended_action") and has("blast_radius")' "$queue" >/dev/null
jq -e 'select(.id == "example-org/example-repo#8" and .kind == "decision")' "$queue" >/dev/null
jq -e '
  select(.id == "example-org/example-repo#9" and .kind == "decision")
  | .mandate.reason
  | startswith("out-of-mandate:")
' "$queue" >/dev/null
jq -e '
  select(.repo == "example-org/another-repo")
  | .id == "example-org/another-repo#20"
    and .kind == "drift"
    and (.mandate.reason | contains("paused project CI"))
' "$queue" >/dev/null
[ "$(jq -s 'length' "$queue")" -eq 4 ]
if grep -q $'example-org/another-repo\tissue list' "$fixture/gh-calls"; then
  echo "paused project was queried for issues" >&2
  exit 1
fi
grep -q $'example-org/another-repo\tpr list' "$fixture/gh-calls"

cp "$queue" "$fixture/queue.before"
cp "$state" "$fixture/state.before"
sleep 1
run_sweep >/dev/null
cmp "$fixture/queue.before" "$queue"
cmp "$fixture/state.before" "$state"

set +e
paused_approval="$(
  CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/queue.sh" approve 'example-org/another-repo#20' 2>&1
)"
paused_approval_status=$?
set -e
[ "$paused_approval_status" -eq 4 ]
grep -q 'is paused; CI drift cannot mint a handoff token' <<<"$paused_approval"
jq -e '
  select(.id == "example-org/another-repo#20" and .state == "pending")
' "$queue" >/dev/null

hook_calls_before="$(wc -l <"$fixture/gh-calls")"
digest="$(
  cd "$fixture/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$fixture/gh-calls" \
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
hook_calls_after="$(wc -l <"$fixture/gh-calls")"
[ "$hook_calls_before" -eq "$hook_calls_after" ]
printf '%s\n' "$digest" | awk '
  NR == 1 && $0 != "DECISIONS WAITING" { exit 1 }
  $0 == "DRIFT" { drift = NR }
  END { exit !(drift > 1) }
'
grep -q 'example-org/example-repo#7 tripwire' <<<"$digest"
if grep -Eq '^(MOVED SINCE|STUCK)' <<<"$digest"; then
  echo "empty digest section was rendered" >&2
  exit 1
fi

CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/queue.sh" approve 'example-org/example-repo#8' |
  grep -q 'approval token mandate:example-org/example-repo#8'
jq -e 'select(.id == "example-org/example-repo#8" and .state == "approved")' "$queue" >/dev/null

CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/scripts/queue.sh" reject 'example-org/example-repo#7' >/dev/null
if jq -e 'select(.id == "example-org/example-repo#7")' "$queue" >/dev/null; then
  echo "rejected row remained in queue" >&2
  exit 1
fi

# A fresh healthy portfolio renders exactly one line, and the hook never
# consults gh even when it is available on PATH.
mkdir -p "$fixture/healthy/config/ostrom" "$fixture/healthy/repo"
cat >"$fixture/healthy/config/ostrom/mandates.yaml" <<'YAML'
projects:
  - repo: example-org/example-repo
    delegated: routine maintenance
    paused: false
    bounce: []
  - repo: example-org/another-repo
    delegated: CI health maintenance
    paused: true
    bounce: []
YAML
cat >"$fixture/healthy/config/ostrom/state.json" <<'JSON'
{"version":1,"repos":{}}
JSON
healthy_calls_before="$(wc -l <"$fixture/gh-calls")"
healthy="$(
  cd "$fixture/healthy/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$fixture/gh-calls" \
    CLAUDE_CONFIG_DIR="$fixture/healthy/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
healthy_calls_after="$(wc -l <"$fixture/gh-calls")"
[ "$healthy" = "2 projects nominal" ]
[ "$healthy_calls_before" -eq "$healthy_calls_after" ]

touch -t 200001010000 "$fixture/healthy/config/ostrom/state.json"
stale_calls_before="$(wc -l <"$fixture/gh-calls")"
stale_digest="$(
  cd "$fixture/healthy/repo"
  PATH="$fixture/bin:$PATH" \
    FAKE_GH_CALL_LOG="$fixture/gh-calls" \
    CLAUDE_CONFIG_DIR="$fixture/healthy/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
stale_calls_after="$(wc -l <"$fixture/gh-calls")"
[ "$(wc -l <<<"$stale_digest")" -eq 2 ]
grep -q '^STALE — mandate sweep overdue$' <<<"$stale_digest"
grep -q '^2 projects nominal$' <<<"$stale_digest"
[ "$stale_calls_before" -eq "$stale_calls_after" ]

empty="$fixture/empty-config"
mkdir -p "$empty"
unconfigured="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$empty" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
[ -z "$unconfigured" ]

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

echo "mandate tests: ok"
