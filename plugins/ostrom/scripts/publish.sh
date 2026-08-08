#!/usr/bin/env bash
# Derive the public governance record tree and publish it to an orphan branch.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

usage() {
  echo "usage: publish.sh [--dry-run]" >&2
  exit 2
}

dry_run=false
case "${1:-}" in
  --dry-run) dry_run=true ;;
  "") ;;
  *) usage ;;
esac
[ "$#" -le 1 ] || usage

for command_name in git jq; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "mandate publish: $command_name is required" >&2
    exit 1
  }
done

allowlist_file="${MANDATE_PUBLISH_ALLOWLIST:-$MANDATE_PLUGIN_ROOT/config/publish-allowlist.json}"
publish_dir="${MANDATE_PUBLISH_DIR:-$MANDATE_DATA_DIR/publish}"
default_publish_repo="ostrom"
default_publish_repo="onsager-ai/${default_publish_repo}-hub"
publish_remote="${MANDATE_PUBLISH_REMOTE:-$default_publish_repo}"
publish_time="${MANDATE_PUBLISH_TIME:-${MANDATE_SWEEP_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}}"
queue_file="$MANDATE_QUEUE_FILE"
gate_file="$MANDATE_GATE_LOG"
state_file="$MANDATE_STATE_FILE"

jq -e '
  type == "object"
  and (._comment | type == "string" and length > 0)
  and (del(._comment) | all(to_entries[];
      (.value | type == "array")
      and (.value | length) > 0
      and (.value | length) == (.value | unique | length)
      and all(.value[]; type == "string" and length > 0)
    ))
' "$allowlist_file" >/dev/null || {
  echo "mandate publish: invalid allowlist at $allowlist_file" >&2
  exit 2
}

for source_file in "$queue_file" "$state_file"; do
  [ -f "$source_file" ] || {
    echo "mandate publish: source record is missing: $source_file" >&2
    exit 2
  }
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/tree/gate"
: >"$work/tree/queue.jsonl"
[ -f "$gate_file" ] || : >"$work/empty-gate.jsonl"
gate_source="$gate_file"
[ -f "$gate_source" ] || gate_source="$work/empty-gate.jsonl"

# The same named allowlist drives both copying and drop accounting. Every
# object is rebuilt from permitted keys; an unknown source key has no route
# into the derived value.
jq -c --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def keep($shape):
    . as $object
    | reduce $allow[$shape][] as $key ({};
        if $object | has($key) then .[$key] = $object[$key] else . end
      );
  def queue_record:
    keep("queue")
    | .mandate = (
        if (.mandate | type) == "object" then
          .mandate
          |
          keep("queue.mandate")
          | if (.dossier | type) == "object"
            then .dossier |= keep("queue.mandate.dossier")
            else del(.dossier)
            end
        else {}
        end
      );
  queue_record
' "$queue_file" >"$work/tree/queue.jsonl"

jq -s --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def unknown($shape; $object; $prefix):
    if ($object | type) != "object" then []
    else [
      $object | keys_unsorted[] as $key
      | select(($allow[$shape] | index($key)) == null)
      | $prefix + $key
    ] end;
  [
    .[] as $record
    | unknown("queue"; $record; "")[],
      unknown("queue.mandate"; $record.mandate; "mandate.")[],
      unknown("queue.mandate.dossier"; $record.mandate.dossier; "mandate.dossier.")[]
  ]
' "$queue_file" >"$work/queue-drops.json"

jq -c --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def keep($shape):
    . as $object
    | reduce $allow[$shape][] as $key ({};
        if $object | has($key) then .[$key] = $object[$key] else . end
      );
  def condition_record:
    . as $source
    | keep("gate.condition")
    | if ($source.detail | type) != "object" then del(.detail)
      elif $source.name == "mergeable" then
        .detail = ($source.detail | keep("gate.detail.mergeable"))
      elif $source.name == "draft" then
        .detail = ($source.detail | keep("gate.detail.draft"))
      elif $source.name == "review_threads" then
        .detail = ($source.detail | keep("gate.detail.review_threads"))
      elif $source.name == "bounce_selectors" then
        .detail = (
          $source.detail
          | keep("gate.detail.bounce_selectors")
          | .matches = (
              if (.matches | type) == "array"
              then [.matches[] | select(type == "object")
                | keep("gate.detail.bounce_selectors.match")]
              else []
              end
            )
          | .unobservable = (
              if (.unobservable | type) == "array"
              then [.unobservable[] | select(type == "object")
                | keep("gate.detail.bounce_selectors.unobservable")]
              else []
              end
            )
        )
      elif $source.name == "reserved_refs" then
        .detail = (
          $source.detail
          | keep("gate.detail.reserved_refs")
          | .matches = (
              if (.matches | type) == "array"
              then [.matches[] | select(type == "string")]
              else []
              end
            )
        )
      elif $source.name == "required_checks" then
        .detail = (
          $source.detail
          | keep("gate.detail.required_checks")
          | .selectors = (
              if (.selectors | type) == "array"
              then [.selectors[] | select(type == "object")
                | keep("gate.detail.required_checks.selector")
                | .matches = (
                    if (.matches | type) == "array"
                    then [.matches[] | select(type == "object")
                      | keep("gate.detail.required_checks.match")]
                    else []
                    end
                  )]
              else []
              end
            )
        )
      else del(.detail)
      end;
  keep("gate")
  | .conditions = (
      if (.conditions | type) == "array"
      then [.conditions[] | select(type == "object") | condition_record]
      else []
      end
    )
' "$gate_source" >"$work/gate.jsonl"

jq -s --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def unknown($shape; $object; $prefix):
    if ($object | type) != "object" then []
    else [
      $object | keys_unsorted[] as $key
      | select(($allow[$shape] | index($key)) == null)
      | $prefix + $key
    ] end;
  def object_array($value):
    if ($value | type) == "array"
    then [$value[] | select(type == "object")]
    else []
    end;
  def detail_unknown($condition):
    if ($condition.detail | type) != "object" then []
    elif $condition.name == "mergeable" then
      unknown("gate.detail.mergeable"; $condition.detail; "conditions[].detail.")
    elif $condition.name == "draft" then
      unknown("gate.detail.draft"; $condition.detail; "conditions[].detail.")
    elif $condition.name == "review_threads" then
      unknown("gate.detail.review_threads"; $condition.detail; "conditions[].detail.")
    elif $condition.name == "bounce_selectors" then
      unknown("gate.detail.bounce_selectors"; $condition.detail; "conditions[].detail.")
      + [object_array($condition.detail.matches)[] as $match
          | unknown("gate.detail.bounce_selectors.match"; $match;
              "conditions[].detail.matches[].")[]]
      + [object_array($condition.detail.unobservable)[] as $entry
          | unknown("gate.detail.bounce_selectors.unobservable"; $entry;
              "conditions[].detail.unobservable[].")[]]
    elif $condition.name == "reserved_refs" then
      unknown("gate.detail.reserved_refs"; $condition.detail; "conditions[].detail.")
    elif $condition.name == "required_checks" then
      unknown("gate.detail.required_checks"; $condition.detail; "conditions[].detail.")
      + [object_array($condition.detail.selectors)[] as $selector
          | unknown("gate.detail.required_checks.selector"; $selector;
              "conditions[].detail.selectors[].")[]]
      + [object_array($condition.detail.selectors)[] as $selector
          | object_array($selector.matches)[] as $match
          | unknown("gate.detail.required_checks.match"; $match;
              "conditions[].detail.selectors[].matches[].")[]]
    else ["conditions[].detail"]
    end;
  [
    .[] as $record
    | unknown("gate"; $record; "")[],
      object_array($record.conditions)[] as $condition
      | unknown("gate.condition"; $condition; "conditions[].")[],
        detail_unknown($condition)[]
  ]
' "$gate_source" >"$work/gate-drops.json"

jq -S --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def keep($shape):
    . as $object
    | reduce $allow[$shape][] as $key ({};
        if $object | has($key) then .[$key] = $object[$key] else . end
      );
  keep("state")
  | .dead_selectors = (
      if (.dead_selectors | type) == "array"
      then [.dead_selectors[] | select(type == "object") | keep("state.dead_selector")]
      else []
      end
    )
  | .repos = (
    (if (.repos | type) == "object" then .repos else {} end)
    | with_entries(
      .value |= (
        if type == "object" then
          keep("state.repo")
          | .items = (
              if (.items | type) == "object"
              then .items | with_entries(
                .value |= if type == "object" then keep("state.item") else {} end
              )
              else {}
              end
            )
          | .policy = (
              if (.policy | type) == "object"
              then .policy | keep("state.policy") else {} end
            )
          | .scope_changes = (
              if (.scope_changes | type) == "object"
              then .scope_changes | keep("state.scope_changes") else {} end
            )
          | .notice = (
              if (.notice | type) == "object"
              then .notice | keep("state.notice") else null end
            )
        else {}
        end
      )
    )
  )
' "$state_file" >"$work/tree/state.json"

jq --slurpfile allow_file "$allowlist_file" '
  $allow_file[0] as $allow
  |
  def unknown($shape; $object; $prefix):
    if ($object | type) != "object" then []
    else [
      $object | keys_unsorted[] as $key
      | select(($allow[$shape] | index($key)) == null)
      | $prefix + $key
    ] end;
  . as $state
  | [
      unknown("state"; $state; "")[],
      ($state.dead_selectors // [])[] as $dead
        | unknown("state.dead_selector"; $dead; "dead_selectors[].")[],
      ($state.repos // {} | to_entries[]) as $repo
        | unknown("state.repo"; $repo.value; "repos.*.")[],
          ($repo.value.items // {} | to_entries[]) as $item
            | unknown("state.item"; $item.value; "repos.*.items.*.")[],
          unknown("state.policy"; $repo.value.policy; "repos.*.policy.")[],
          unknown("state.scope_changes"; $repo.value.scope_changes; "repos.*.scope_changes.")[],
          unknown("state.notice"; $repo.value.notice; "repos.*.notice.")[]
    ]
' "$state_file" >"$work/state-drops.json"

publish_day="${publish_time%%T*}"
cutoff_day="$(jq -nr --arg timestamp "${publish_day}T00:00:00Z" '
  ($timestamp | fromdateiso8601) - (89 * 24 * 60 * 60) | strftime("%Y-%m-%d")
')" || {
  echo "mandate publish: invalid publish timestamp: $publish_time" >&2
  exit 2
}

if [ -s "$work/gate.jsonl" ]; then
  jq -r '.ts[0:10]' "$work/gate.jsonl" | sort -u >"$work/gate-days"
  while IFS= read -r day; do
    [[ "$day" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || {
      echo "mandate publish: gate record has an invalid timestamp day: $day" >&2
      exit 2
    }
    if [[ "$day" < "$cutoff_day" ]]; then
      continue
    fi
    jq -c --arg day "$day" 'select(.ts[0:10] == $day)' "$work/gate.jsonl" \
      >"$work/tree/gate/$day.jsonl"
  done <"$work/gate-days"
fi

jq -S -n \
  --slurpfile queue "$work/tree/queue.jsonl" \
  --slurpfile gate "$work/gate.jsonl" \
  --slurpfile state "$work/tree/state.json" '
  def increment($key): .[$key] = ((.[$key] // 0) + 1);
  def age_bucket($age):
    if $age <= 1 then "0-1"
    elif $age <= 7 then "2-7"
    elif $age <= 30 then "8-30"
    else "31+"
    end;
  {
    verdicts_by_day: (
      reduce $gate[] as $record ({};
        .[$record.ts[0:10]] = (
          .[$record.ts[0:10]] // {pass: 0, fail: 0, inconclusive: 0}
        )
        |
        .[$record.ts[0:10]][$record.verdict] =
          ((.[$record.ts[0:10]][$record.verdict] // 0) + 1)
      )
    ),
    queue_age_buckets: (
      reduce $queue[] as $record (
        {"0-1": 0, "2-7": 0, "8-30": 0, "31+": 0};
        increment(age_bucket($record.age_days))
      )
    ),
    repo_classifications: (
      reduce (($state[0].repos // {}) | to_entries[]) as $repo ({};
        .[$repo.key] = (
          reduce (($repo.value.items // {}) | to_entries[]) as $item (
            {delegated: 0, reserved: 0, excluded: 0, tripwire: 0, unclassified: 0};
            increment($item.value.classification)
          )
        )
      )
    )
  }
' >"$work/tree/rollup.json"

schema_id="$(jq -S -c 'del(._comment)' "$allowlist_file" | git hash-object --stdin)"
jq -S -n \
  --arg schema_id "git:$schema_id" \
  --arg published_at "$publish_time" \
  --argjson cadence "$(mandate_load_config | jq '.cadence_hours')" \
  --argjson queue_count "$(wc -l <"$work/tree/queue.jsonl" | tr -d '[:space:]')" \
  --argjson gate_count "$(wc -l <"$work/gate.jsonl" | tr -d '[:space:]')" \
  --argjson repo_count "$(jq '.repos | length' "$work/tree/state.json")" \
  --argjson partition_count "$(find "$work/tree/gate" -type f -name '*.jsonl' | wc -l | tr -d '[:space:]')" \
  --slurpfile queue_drops "$work/queue-drops.json" \
  --slurpfile gate_drops "$work/gate-drops.json" \
  --slurpfile state_drops "$work/state-drops.json" '
  def counts($values):
    reduce $values[] as $field ({};
      .[$field] = ((.[$field] // 0) + 1)
    );
  {
    schema_id: $schema_id,
    published_at: $published_at,
    expected_sweep_interval_hours: $cadence,
    retention: {gate_days: 90, rollup: "forever"},
    record_counts: {
      queue: $queue_count,
      gate: $gate_count,
      state_repos: $repo_count,
      gate_partitions: $partition_count
    },
    dropped_fields: {
      queue: counts($queue_drops[0]),
      gate: counts($gate_drops[0]),
      state: counts($state_drops[0])
    }
  }
' >"$work/tree/manifest.json"

print_tree() {
  jq -S -n \
    --slurpfile manifest "$work/tree/manifest.json" \
    --slurpfile queue "$work/tree/queue.jsonl" \
    --slurpfile state "$work/tree/state.json" \
    --slurpfile rollup "$work/tree/rollup.json" '
      {
        "manifest.json": $manifest[0],
        "queue.jsonl": $queue,
        "state.json": $state[0],
        "rollup.json": $rollup[0]
      }
    ' >"$work/printed-tree.json"
  for partition in "$work"/tree/gate/*.jsonl; do
    [ -e "$partition" ] || continue
    path="gate/${partition##*/}"
    jq -S --arg path "$path" --slurpfile rows "$partition" \
      '.[$path] = $rows' "$work/printed-tree.json" >"$work/next-tree.json"
    mv "$work/next-tree.json" "$work/printed-tree.json"
  done
  jq -S . "$work/printed-tree.json"
}

if $dry_run; then
  print_tree
  exit 0
fi

mkdir -p "$(dirname "$publish_dir")"
if [ ! -d "$publish_dir/.git" ]; then
  if [[ "$publish_remote" == */* && "$publish_remote" != /* && "$publish_remote" != *:* ]]; then
    command -v gh >/dev/null 2>&1 || {
      echo "mandate publish: gh is required to clone an owner/repo remote; pass a URL or path instead to skip it" >&2
      exit 1
    }
    gh repo clone "$publish_remote" "$publish_dir" -- --no-checkout >/dev/null
  else
    git clone --no-checkout "$publish_remote" "$publish_dir" >/dev/null
  fi
fi

if git -C "$publish_dir" ls-remote --exit-code --heads origin state >/dev/null 2>&1; then
  git -C "$publish_dir" fetch --quiet origin state
  git -C "$publish_dir" checkout -B state FETCH_HEAD >/dev/null
else
  if git -C "$publish_dir" checkout --orphan state >/dev/null 2>&1; then
    # `checkout --orphan` (unlike `switch --orphan`) inherits whatever is
    # currently staged. A `--no-checkout` clone still populates the index
    # from the remote's default branch even though it skips the working
    # tree, so clear it explicitly: the first publish commit must hold only
    # the derived tree, never a stray entry carried over from that branch.
    git -C "$publish_dir" read-tree --empty
  else
    git -C "$publish_dir" checkout state >/dev/null
  fi
fi

# Reuse the branch timestamp when every non-manifest byte and every stable
# manifest field is unchanged. This turns a true no-op into an empty index.
if [ -f "$publish_dir/manifest.json" ]; then
  previous_time="$(jq -r '.published_at // empty' "$publish_dir/manifest.json" 2>/dev/null || true)"
  if [ -n "$previous_time" ]; then
    jq --arg published_at "$previous_time" '.published_at = $published_at' \
      "$work/tree/manifest.json" >"$work/manifest.previous-time.json"
    if cmp -s "$work/manifest.previous-time.json" "$publish_dir/manifest.json"; then
      non_manifest_changed=false
      for path in queue.jsonl state.json rollup.json; do
        cmp -s "$work/tree/$path" "$publish_dir/$path" || non_manifest_changed=true
      done
      current_gate="$(find "$work/tree/gate" -type f -name '*.jsonl' -exec basename {} \; | sort)"
      previous_gate="$(find "$publish_dir/gate" -type f -name '*.jsonl' -exec basename {} \; 2>/dev/null | sort)"
      [ "$current_gate" = "$previous_gate" ] || non_manifest_changed=true
      if ! $non_manifest_changed; then
        while IFS= read -r name; do
          [ -n "$name" ] || continue
          cmp -s "$work/tree/gate/$name" "$publish_dir/gate/$name" || non_manifest_changed=true
        done <<<"$current_gate"
      fi
      if ! $non_manifest_changed; then
        mv "$work/manifest.previous-time.json" "$work/tree/manifest.json"
      fi
    fi
  fi
fi

git -C "$publish_dir" rm -r --quiet --ignore-unmatch \
  manifest.json queue.jsonl state.json rollup.json gate
mkdir -p "$publish_dir/gate"
cp "$work/tree/manifest.json" "$publish_dir/manifest.json"
cp "$work/tree/queue.jsonl" "$publish_dir/queue.jsonl"
cp "$work/tree/state.json" "$publish_dir/state.json"
cp "$work/tree/rollup.json" "$publish_dir/rollup.json"
cp "$work/tree/gate"/*.jsonl "$publish_dir/gate/" 2>/dev/null || true
git -C "$publish_dir" add manifest.json queue.jsonl state.json rollup.json gate

if git -C "$publish_dir" diff --cached --quiet; then
  echo "mandate publish: unchanged"
  exit 0
fi

git -C "$publish_dir" commit --quiet -m "chore(state): publish governance snapshot $publish_time"
git -C "$publish_dir" push --quiet origin HEAD:state
echo "mandate publish: published $publish_time"
