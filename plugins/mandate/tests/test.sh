#!/usr/bin/env bash

set -euo pipefail

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
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
      issue7_title="feat(tooling): improve runner"
      if [ "${FAKE_GH_MODE:-base}" = "changed" ]; then
        issue7_title="feat(tooling): improve runner title refreshed upstream"
      fi
      cat <<JSON
[{"number":7,"title":"$issue7_title","labels":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/7"},{"number":9,"title":"Untriaged request","labels":[],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/9"},{"number":10,"title":"feat(tooling): owner gate","body":"Depends on #7.","labels":[{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/10"},{"number":11,"title":"Path-only issue","labels":[],"files":[{"path":"docs/guide.md"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/11"},{"number":14,"title":"Rotate credential safely","labels":[{"name":"ignored"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/14"},{"number":15,"title":"Routine excluded work","labels":[{"name":"ignored"},{"name":"maintenance"}],"createdAt":"2026-07-29T00:00:00Z","updatedAt":"2026-07-30T00:00:00Z","url":"https://example.invalid/issues/15"}]
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
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "pr list" ]; then
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
    and .stuck == true
    and (.blocked_by | type) == "array"
  )
  and any(.[];
    .id == "example-org/example-repo#10"
    and .kind == "decision"
    and .needs_judgment == true
    and .blocked_by == ["example-org/example-repo#7"]
  )
  and any(.[];
    .id == "example-org/example-repo#12"
    and .kind == "tripwire"
    and .needs_judgment == true
    and .blocked_by == ["example-org/another-repo#20"]
  )
  and any(.[];
    .id == "example-org/example-repo#13"
    and .kind == "drift"
    and .needs_judgment == false
    and .blocked_by == []
  )
' "$queue" >/dev/null

# /desk reads the same titled records rather than making every number another
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
jq -c 'del(.age_days, .stuck, .needs_judgment, .blocked_by)' "$queue" \
  >"$legacy_config/ostrom/queue.jsonl"
legacy_rows="$(
  CLAUDE_CONFIG_DIR="$legacy_config" CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/scripts/queue.sh" list
)"
jq -s -e '
  length == 5
  and all(.[];
    (has("age_days") | not)
    and (has("stuck") | not)
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
grep -q "^Produce today's /brief now\." <<<"$digest_text"
[ -f "$fixture/config/ostrom/.tap-2026-08-01" ]
grep -q \
  '^example-org/example-repo#10  feat(tooling): owner gate — reserved ref:#10$' \
  <<<"$digest_text"
grep -q \
  '^example-org/example-repo#13  (title unavailable) — CI is failing; default:unclassified$' \
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
grep -q '^example-org/example-repo: 3 unclassified — /desk triage$' <<<"$digest_text"
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
grep -q '^example-org/rollup-one: 3 unclassified — /desk triage$' \
  <<<"$rollup_digest_text"
grep -q '^example-org/rollup-two: 2 unclassified — /desk triage$' \
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

echo "mandate tests: ok"
