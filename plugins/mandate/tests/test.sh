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
' <<<"$layered" >/dev/null

cat >"$fixture/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "$1 $2" = "auth status" ]; then
  [ "${FAKE_GH_AUTH_FAIL:-0}" != "1" ]
  exit 0
fi
if [ "$1 $2" = "issue list" ]; then
  cat <<'JSON'
[{"number":7,"title":"Plan a production deployment","labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/7"}]
JSON
  exit 0
fi
if [ "$1 $2" = "pr list" ]; then
  cat <<'JSON'
[{"number":8,"title":"Routine maintenance","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/8","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}]}]
JSON
  exit 0
fi
exit 1
EOF
chmod +x "$fixture/bin/gh"

run_sweep() {
  (
    cd "$fixture/repo"
    PATH="$fixture/bin:$PATH" \
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

cp "$queue" "$fixture/queue.before"
cp "$state" "$fixture/state.before"
sleep 1
run_sweep >/dev/null
cmp "$fixture/queue.before" "$queue"
cmp "$fixture/state.before" "$state"

digest="$(
  cd "$fixture/repo"
  CLAUDE_CONFIG_DIR="$fixture/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    MANDATE_SKIP_SWEEP=1 \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
printf '%s\n' "$digest" | awk '
  NR == 1 && $0 != "DECISIONS WAITING" { exit 1 }
  /^MOVED SINCE / { moved = NR }
  $0 == "STUCK" { stuck = NR }
  $0 == "DRIFT" { drift = NR }
  END { exit !(moved > 1 && stuck > moved && drift > stuck) }
'
grep -q 'example-org/example-repo#7 tripwire' <<<"$digest"

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
