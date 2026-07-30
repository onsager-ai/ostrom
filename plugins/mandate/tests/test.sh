#!/usr/bin/env bash

set -euo pipefail

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/config/ostrom" "$fixture/repo" "$fixture/bin"

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

# Prove shipped < user < repo precedence and generic project list parsing.
mkdir -p "$fixture/layers/config/ostrom" "$fixture/layers/repo/.ostrom"
cat >"$fixture/layers/config/ostrom/mandates.yaml" <<'YAML'
cadence_hours: 12
stuck_after_days: 5
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
      cat <<'JSON'
[{"number":7,"title":"feat(tooling): improve runner","labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/7"},{"number":9,"title":"Untriaged request","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/9"},{"number":10,"title":"feat(tooling): owner gate","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/10"},{"number":11,"title":"Path-only issue","labels":[],"files":[{"path":"docs/guide.md"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/11"},{"number":14,"title":"Rotate credential safely","labels":[{"name":"ignored"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/14"},{"number":15,"title":"Routine excluded work","labels":[{"name":"ignored"},{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/15"}]
JSON
      ;;
    example-org/another-repo)
      cat <<'JSON'
[{"number":20,"title":"Prepare production deployment","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/20"}]
JSON
      ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "pr list" ]; then
  case "$repo" in
    example-org/example-repo)
      changed_at="2026-07-30T00:00:00Z"
      [ "${FAKE_GH_MODE:-base}" = "changed" ] && changed_at="2026-08-01T00:00:00Z"
      cat <<JSON
[{"number":8,"title":"fix: routine maintenance","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"$changed_at","url":"https://example.invalid/pull/8","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[{"number":42,"labels":[{"name":"maintenance"}]}],"files":[{"path":"src/main.sh"}]},{"number":12,"title":"chore: update frozen rule","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/12","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"closingIssuesReferences":[],"files":[{"path":"rules/frozen-rules.md"}]},{"number":13,"title":"fix: broken checks","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/13","isDraft":false,"reviewDecision":"","statusCheckRollup":[{"conclusion":"FAILURE","status":"COMPLETED"}],"closingIssuesReferences":[],"files":[]},{"number":16,"title":"docs: nested guide","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/pull/16","isDraft":true,"reviewDecision":"","statusCheckRollup":[],"closingIssuesReferences":[],"files":[{"path":"docs/reference/deep/guide.md"}]}]
JSON
      ;;
    *) echo '[]' ;;
  esac
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
      FAKE_GH_MODE="${FAKE_GH_MODE:-base}" \
      CLAUDE_CONFIG_DIR="$fixture/config" \
      CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
      bash "$PLUGIN_ROOT/scripts/sweep.sh"
  )
}

# The first sweep is a baseline. Only reserved, tripwire, and drift carve-outs
# queue; the paused project's issue tripwire still fires.
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
  select(.id == "example-org/example-repo#13" and .kind == "drift")
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
grep -q '^example-org/example-repo: baselined 10 open items$' <<<"$digest"
grep -q '^example-org/another-repo: baselined 1 open items$' <<<"$digest"
grep -q '^example-org/example-repo: 3 unclassified — /desk triage$' <<<"$digest"

# The hook stays local, and a repeat sweep with no upstream movement is a
# serialized no-op.
hook_calls_before="$(wc -l <"$fixture/gh-calls")"
CLAUDE_CONFIG_DIR="$fixture/config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
  bash "$PLUGIN_ROOT/hooks/render-digest.sh" >/dev/null
hook_calls_after="$(wc -l <"$fixture/gh-calls")"
[ "$hook_calls_before" -eq "$hook_calls_after" ]
cp "$queue" "$fixture/queue.before"
cp "$state" "$fixture/state.before"
run_sweep >/dev/null
cmp "$fixture/queue.before" "$queue"
cmp "$fixture/state.before" "$state"

# A later PR update produces one ready decision, proving inherited labels are
# used by the live (non-baseline) classifier too.
FAKE_GH_MODE=changed run_sweep >/dev/null
jq -e '
  select(.id == "example-org/example-repo#8" and .kind == "decision")
  | .mandate.reason
  | startswith("delegated label:maintenance;")
' "$queue" >/dev/null

# Editing a selector re-baselines scope: one item enters, one leaves, neither
# is emitted as a routine row, and the detail is durable for /desk.
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
grep -q 'mandate changed — 1 items entered scope, 1 left' <<<"$policy_digest"

# A selector that matched nowhere in the complete sweep is audible.
jq -e '
  .dead_selectors
  | any(.source == "bounce_all" and .selector == "title:*never fires*")
' "$state" >/dev/null
grep -q '^dead selector — bounce_all title:\*never fires\*$' <<<"$policy_digest"

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
[ "$(wc -l <<<"$portfolio_digest")" -le 20 ]

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
[ "$healthy" = "2 projects nominal" ]

touch -t 200001010000 "$fixture/healthy/config/ostrom/state.json"
stale_digest="$(
  cd "$fixture/healthy/repo"
  CLAUDE_CONFIG_DIR="$fixture/healthy/config" \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/render-digest.sh"
)"
[ "$(wc -l <<<"$stale_digest")" -eq 2 ]
grep -q '^STALE — mandate sweep overdue$' <<<"$stale_digest"
grep -q '^2 projects nominal$' <<<"$stale_digest"

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
