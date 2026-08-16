#!/usr/bin/env bash
# Shared config and queue helpers for the mandate subsystem.

MANDATE_PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
MANDATE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
MANDATE_DATA_DIR="$MANDATE_CONFIG_DIR/ostrom"
MANDATE_SECRETS_FILE="$MANDATE_DATA_DIR/secrets.yaml"
MANDATE_USER_CONFIG="$MANDATE_DATA_DIR/mandates.yaml"
MANDATE_REPO_CONFIG="./.ostrom/mandates.yaml"
MANDATE_DEFAULT_CONFIG="$MANDATE_PLUGIN_ROOT/config/mandate-defaults.yaml"
MANDATE_QUEUE_FILE="$MANDATE_DATA_DIR/queue.jsonl"
MANDATE_STATE_FILE="$MANDATE_DATA_DIR/state.json"
MANDATE_PLAN_FILE="$MANDATE_DATA_DIR/plan.json"
MANDATE_EVENTS_FILE="$MANDATE_DATA_DIR/selector-events.jsonl"
MANDATE_GATE_DEFAULT_CONFIG="$MANDATE_PLUGIN_ROOT/config/gate.defaults.yaml"
MANDATE_GATE_USER_CONFIG="$MANDATE_DATA_DIR/gate.yaml"
MANDATE_GATE_REPO_CONFIG="./.ostrom/gate.yaml"
MANDATE_GATE_LOG="$MANDATE_DATA_DIR/gate.jsonl"
MANDATE_EXCEPTIONS_LOG="$MANDATE_DATA_DIR/exceptions.jsonl"

# Semantic derivation is an optional port. An explicit executable is useful
# for hermetic fixtures and alternate providers; otherwise the bundled
# adapter is enabled only by Anthropic's standard credential. Neither value
# is part of mandate policy, and absence preserves the mechanical sweep.
mandate_semantic_is_configured() {
  [ -n "${MANDATE_SEMANTIC_DERIVER:-}" ] || [ -n "${ANTHROPIC_API_KEY:-}" ]
}

# Read a delivery role's GitHub App credentials from the machine-local secrets
# file, preferring a role block so the shared-App cutover stays reversible.
# This intentionally remains separate from the shipped/user/repository config
# layers: credentials have no repository or shipped layer.
mandate_load_role_credentials() {
  if [ "$#" -ne 1 ] || [ -z "${1:-}" ]; then
    echo "app-token: mandate_load_role_credentials requires exactly one role" >&2
    return 2
  fi
  local role="$1"
  local credential_name credential_records credentials required_field
  case "$role" in
    [!a-z]*|*[!a-z0-9_-]*)
      echo "app-token: invalid role: must match [a-z][a-z0-9_-]*" >&2
      return 2
      ;;
  esac

  if [ ! -f "$MANDATE_SECRETS_FILE" ]; then
    echo "app-token: secrets file is missing at the configured Ostrom secrets path" >&2
    return 2
  fi

  credential_name="$(
    awk -v role="$role" '
      $0 == role ":" { found_role = 1 }
      $0 == "shared:" { found_shared = 1 }
      END {
        if (found_role) print role
        else if (found_shared) print "shared"
        else exit 2
      }
    ' "$MANDATE_SECRETS_FILE"
  )" || {
    echo "app-token: neither $role nor shared credentials are configured" >&2
    return 2
  }

  credential_records="$(
    awk -v credential_name="$credential_name" '
      function trim(s) {
        sub(/^[[:space:]]+/, "", s)
        sub(/[[:space:]]+$/, "", s)
        return s
      }
      function unquote(s, first, last) {
        s = trim(s)
        first = substr(s, 1, 1)
        last = substr(s, length(s), 1)
        if (length(s) >= 2 && ((first == "\"" && last == "\"") || (first == "\047" && last == "\047"))) {
          s = substr(s, 2, length(s) - 2)
        }
        return s
      }
      function fail(message) {
        printf "app-token: could not parse %s credentials: %s\n", credential_name, message > "/dev/stderr"
        failed = 1
      }
      BEGIN { in_credentials = 0; found_credentials = 0; failed = 0 }
      {
        raw = $0
        if (raw ~ /\t/) {
          if (in_credentials) fail("tabs are not supported")
          next
        }
        if (raw ~ /^[[:space:]]*#/ || raw ~ /^[[:space:]]*$/) next

        match(raw, /^ */)
        indent = RLENGTH
        text = substr(raw, indent + 1)

        if (indent == 0) {
          in_credentials = (text == credential_name ":")
          if (in_credentials) found_credentials = 1
          next
        }
        if (!in_credentials) next

        sub(/[[:space:]]+#.*$/, "", text)
        if (text ~ /^[[:space:]]*$/) next
        if (indent != 2 || text !~ /^(app_id|installation_id|private_key_path):[[:space:]]*/) {
          fail("unsupported " credential_name " entry")
          next
        }

        key = text
        sub(/:.*/, "", key)
        # installation_id is obsolete. Accept it for compatibility, but do
        # not validate or return it as part of the credentials schema.
        if (key == "installation_id") next
        value = text
        sub(/^[^:]+:[[:space:]]*/, "", value)
        value = unquote(value)
        if (seen[key]++) {
          fail("duplicate " key " field")
        } else if (value == "") {
          fail("empty " key " field")
        } else {
          print key "\t" value
        }
      }
      END {
        if (!found_credentials && !failed) {
          printf "app-token: %s credentials are not configured\n", credential_name > "/dev/stderr"
          exit 2
        }
        if (failed) exit 2
      }
    ' "$MANDATE_SECRETS_FILE"
  )" || return

  credentials="$(
    printf '%s\n' "$credential_records" |
      jq -Rn '
        reduce inputs as $line ({};
          ($line | split("\t")) as $parts
          | .[$parts[0]] = $parts[1]
        )
      '
  )" || {
    echo "app-token: could not parse $credential_name credentials" >&2
    return 2
  }

  for required_field in app_id private_key_path; do
    if ! jq -e --arg field "$required_field" \
      '.[$field] | type == "string" and length > 0' \
      >/dev/null <<<"$credentials"; then
      echo "app-token: missing required $credential_name field: $required_field" >&2
      return 2
    fi
  done

  if ! jq -e '
    .app_id | test("^[1-9][0-9]*$")
  ' >/dev/null <<<"$credentials"; then
    echo "app-token: $credential_name app_id must be a positive integer" >&2
    return 2
  fi

  printf '%s\n' "$credentials"
}

mandate_is_configured() {
  [ -f "$MANDATE_USER_CONFIG" ] || [ -f "$MANDATE_REPO_CONFIG" ]
}

# Parse the deliberately small shipped schema without pretending to be a
# general YAML parser. Supported input:
#   root scalars; bounce_all:/hold_labels:/work_ranking:/search_roots: followed by two-space list items; projects:
#   followed by "- repo:" entries, each with default + paused scalars, an
#   optional max_implementers_per_repository positive integer, and six-space
#   delegated/excluded/reserved/bounce lists.
mandate_yaml_to_json() {
  file="$1"
  [ -f "$file" ] || {
    printf '{}\n'
    return 0
  }

  awk '
    function trim(s) {
      sub(/^[[:space:]]+/, "", s)
      sub(/[[:space:]]+$/, "", s)
      return s
    }
    function unquote(s, first, last) {
      s = trim(s)
      first = substr(s, 1, 1)
      last = substr(s, length(s), 1)
      if (length(s) >= 2 && ((first == "\"" && last == "\"") || (first == "\047" && last == "\047"))) {
        s = substr(s, 2, length(s) - 2)
      }
      return s
    }
    function fail(message) {
      printf "%s:%d: mandate config: %s\n", FILENAME, NR, message > "/dev/stderr"
      failed = 1
    }
    BEGIN { section = ""; current_repo = ""; project_list = ""; failed = 0 }
    {
      raw = $0
      if (raw ~ /\t/) {
        fail("tabs are not supported")
        next
      }
      sub(/[[:space:]]+#.*$/, "", raw)
      if (raw ~ /^[[:space:]]*$/ || raw ~ /^[[:space:]]*#/) next

      match(raw, /^ */)
      indent = RLENGTH
      text = substr(raw, indent + 1)

      if (indent == 0) {
        section = ""
        current_repo = ""
        project_list = ""
        if (text == "bounce_all:") {
          section = "bounce_all"
          print "array\tbounce_all"
        } else if (text == "bounce_all: []") {
          print "array\tbounce_all"
        } else if (text == "hold_labels:") {
          section = "hold_labels"
          print "array\thold_labels"
        } else if (text == "hold_labels: []") {
          print "array\thold_labels"
        } else if (text == "work_ranking:") {
          section = "work_ranking"
          print "array\twork_ranking"
        } else if (text == "work_ranking: []") {
          print "array\twork_ranking"
        } else if (text == "search_roots:") {
          section = "search_roots"
          print "array\tsearch_roots"
        } else if (text == "search_roots: []") {
          print "array\tsearch_roots"
        } else if (text == "projects:") {
          section = "projects"
          print "array\tprojects"
        } else if (text == "projects: []") {
          print "array\tprojects"
        } else if (text ~ /^(provider|cadence_hours|stuck_after_days):[[:space:]]*/) {
          key = text
          sub(/:.*/, "", key)
          value = text
          sub(/^[^:]+:[[:space:]]*/, "", value)
          value = unquote(value)
          if (value == "") fail("empty scalar " key)
          else print "scalar\t" key "\t" value
        } else {
          fail("unsupported root entry: " text)
        }
        next
      }

      if (section == "bounce_all" && indent == 2 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (value == "") fail("empty bounce_all entry")
        else print "bounce_all\t" value
        next
      }

      if (section == "hold_labels" && indent == 2 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (value == "") fail("empty hold_labels entry")
        else print "hold_label\t" value
        next
      }

      if (section == "work_ranking" && indent == 2 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (value == "") fail("empty work_ranking entry")
        else print "work_rank\t" value
        next
      }

      if (section == "search_roots" && indent == 2 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (value == "") fail("empty search_roots entry")
        else print "search_root\t" value
        next
      }

      if (section == "projects" && indent == 2 && text ~ /^-[[:space:]]+repo:[[:space:]]*/) {
        value = text
        sub(/^-[[:space:]]+repo:[[:space:]]*/, "", value)
        current_repo = unquote(value)
        project_list = ""
        if (current_repo == "") fail("empty project repo")
        else print "project\t" current_repo
        next
      }

      if (section == "projects" && current_repo != "" && indent == 4 && text ~ /^paused:[[:space:]]*/) {
        value = text
        sub(/^paused:[[:space:]]*/, "", value)
        value = unquote(value)
        if (value != "true" && value != "false") fail("paused must be true or false for " current_repo)
        else print "project_field\t" current_repo "\tpaused\t" value
        next
      }
      if (section == "projects" && current_repo != "" && indent == 4 && text ~ /^default:[[:space:]]*/) {
        value = text
        sub(/^default:[[:space:]]*/, "", value)
        value = unquote(value)
        if (value !~ /^(delegated|excluded|unclassified)$/) {
          fail("default must be delegated, excluded, or unclassified for " current_repo)
        } else {
          print "project_field\t" current_repo "\tdefault\t" value
        }
        next
      }
      if (section == "projects" && current_repo != "" && indent == 4 &&
          text ~ /^max_implementers_per_repository:[[:space:]]*/) {
        value = text
        sub(/^max_implementers_per_repository:[[:space:]]*/, "", value)
        value = unquote(value)
        if (value !~ /^[1-9][0-9]*$/) {
          fail("max_implementers_per_repository must be a positive integer for " current_repo)
        } else {
          print "project_field\t" current_repo "\tmax_implementers_per_repository\t" value
        }
        next
      }
      if (section == "projects" && current_repo != "" && indent == 4 &&
          (text ~ /^(delegated|excluded|reserved|bounce):$/ ||
           text ~ /^(delegated|excluded|reserved|bounce): \[\]$/)) {
        project_list = text
        sub(/:.*/, "", project_list)
        print "project_array\t" current_repo "\t" project_list
        next
      }
      if (section == "projects" && current_repo != "" && indent == 6 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (project_list == "") fail("project list entry has no list heading for " current_repo)
        else if (value == "") fail("empty project " project_list " entry")
        else print "project_list\t" current_repo "\t" project_list "\t" value
        next
      }

      fail("unsupported indentation or entry: " text)
    }
    END { if (failed) exit 2 }
  ' "$file" |
    jq -Rn '
      reduce inputs as $line ({};
        ($line | split("\t")) as $parts
        | if $parts[0] == "scalar" then
            if ($parts[1] == "cadence_hours" or $parts[1] == "stuck_after_days")
            then .[$parts[1]] = ($parts[2] | tonumber)
            else .[$parts[1]] = $parts[2]
            end
          elif $parts[0] == "array" then
            .[$parts[1]] = []
          elif $parts[0] == "bounce_all" then
            .bounce_all += [$parts[1]]
          elif $parts[0] == "hold_label" then
            .hold_labels += [$parts[1]]
          elif $parts[0] == "work_rank" then
            .work_ranking += [$parts[1]]
          elif $parts[0] == "search_root" then
            .search_roots += [$parts[1]]
          elif $parts[0] == "project" then
            .projects += [{
              "repo": $parts[1],
              "paused": false,
              "default": "unclassified",
              "delegated": [],
              "excluded": [],
              "reserved": [],
              "bounce": []
            }]
          elif $parts[0] == "project_array" then
            (.projects | map(.repo) | index($parts[1])) as $index
            | if $index == null
              then error("project list appeared before its repo")
              else .projects[$index][$parts[2]] = []
              end
          elif $parts[0] == "project_field" then
            (.projects | map(.repo) | index($parts[1])) as $index
            | if $index == null then error("project field appeared before its repo")
              elif $parts[2] == "paused"
              then .projects[$index].paused = ($parts[3] == "true")
              elif $parts[2] == "max_implementers_per_repository"
              then .projects[$index][$parts[2]] = ($parts[3] | tonumber)
              else .projects[$index][$parts[2]] = $parts[3]
              end
          elif $parts[0] == "project_list" then
            (.projects | map(.repo) | index($parts[1])) as $index
            | if $index == null
              then error("project list entry appeared before its repo")
              elif $parts[2] == "reserved"
              then .projects[$index].reserved += [
                ($parts[3] | ltrimstr("#") | tonumber)
              ]
              else .projects[$index][$parts[2]] += [$parts[3]]
              end
          else error("unknown parser record")
          end
      )
    '
}

mandate_load_config() {
  shipped="$(mandate_yaml_to_json "$MANDATE_DEFAULT_CONFIG")" || return
  user="$(mandate_yaml_to_json "$MANDATE_USER_CONFIG")" || return
  repo="$(mandate_yaml_to_json "$MANDATE_REPO_CONFIG")" || return

  config="$(
    jq -cn \
      --argjson shipped "$shipped" \
      --argjson user "$user" \
      --argjson repo "$repo" \
      '$shipped * $user * $repo | .hold_labels //= [] | .work_ranking //= []'
  )" || return

  if ! jq -e '
    .provider == "file"
    and (.cadence_hours | type == "number" and . > 0 and . == floor)
    and (.stuck_after_days | type == "number" and . >= 0)
    and (.bounce_all | type == "array" and all(.[]; type == "string" and length > 0))
    and (.hold_labels | type == "array" and all(.[]; type == "string" and length > 0))
    and (.work_ranking | type == "array"
      and all(.[];
        type == "string"
        and test("^[^/[:space:]#]+/[^/[:space:]#]+#[1-9][0-9]*$")
      )
      and length == (unique | length))
    and (.search_roots | type == "array" and all(.[]; type == "string" and length > 0))
    and (.projects | type == "array")
    and all(.projects[];
      (.repo | type == "string" and test("^[^/[:space:]]+/[^/[:space:]]+$"))
      and (.paused | type == "boolean")
      and (.default | IN("delegated", "excluded", "unclassified"))
      and (.delegated | type == "array" and all(.[]; type == "string" and length > 0))
      and (.excluded | type == "array" and all(.[]; type == "string" and length > 0))
      and (.reserved | type == "array" and all(.[]; type == "number" and . > 0 and . == floor))
      and (.bounce | type == "array" and all(.[]; type == "string" and length > 0))
      and ((.max_implementers_per_repository // 1)
        | type == "number" and . > 0 and . == floor)
    )
    and (([.projects[].repo] | length) == ([.projects[].repo] | unique | length))
  ' >/dev/null <<<"$config"; then
    echo "mandate: invalid config; provider must be file, cadence_hours a positive integer, hold_labels and search_roots non-empty strings, work_ranking unique owner/repo#N item IDs, and every project must have a unique owner/name repo, valid default, boolean paused value, selector lists, positive integer reserved refs, and an optional positive max_implementers_per_repository" >&2
    return 2
  fi

  selector_error="$(
    jq -r '
      def selector_records:
        (.bounce_all[]? | {where: "bounce_all", selector: .}),
        (.projects[]? as $project
          | ($project.delegated[]? | {
              where: ($project.repo + " delegated"), selector: .
            }),
            ($project.excluded[]? | {
              where: ($project.repo + " excluded"), selector: .
            }),
            ($project.bounce[]? | {
              where: ($project.repo + " bounce"), selector: .
            })
        );
      def lint:
        .selector as $selector
        | ((try ($selector | capture("^(?<prefix>[^:]+):(?<glob>.*)$")) catch null) // null) as $parsed
        | if $parsed == null or
             ($parsed.prefix | IN("label", "scope", "type", "path", "ref", "title") | not)
          then "unknown selector prefix"
          elif $parsed.glob == ""
          then "selector value is empty"
          elif $parsed.prefix == "ref" and
               ($parsed.glob | test("^#[1-9][0-9]*$") | not)
          then "ref selector must be ref:#N"
          elif $parsed.prefix == "title" and
               ($parsed.glob | contains("*") | not)
          then "title selector must contain *"
          elif $parsed.prefix == "title" and
               (($parsed.glob | split("*") | map(length) | max) > 24)
          then "title selector literal run exceeds 24 characters"
          else empty
          end;
      first(selector_records as $record
        | ($record | lint) as $message
        | "\($record.where) selector \"\($record.selector)\": \($message)"
      ) // empty
    ' <<<"$config"
  )" || return
  if [ -n "$selector_error" ]; then
    echo "mandate: invalid config: $selector_error" >&2
    return 2
  fi

  printf '%s\n' "$config"
}

# Return the collision cap for one roster repository. Capacity remains the
# dispatcher's global concern; this project value only prevents concurrent
# implementers from creating branches that can collide in the same repository.
mandate_project_max_implementers_per_repository() {
  local repository="$1"
  local config="${2:-}"
  local default_limit="${3:-1}"

  if [ -z "$config" ]; then
    config="$(mandate_load_config)" || return
  fi

  jq -er --arg repository "$repository" --argjson default "$default_limit" '
    ([.projects[]?
      | select(.repo == $repository)
      | .max_implementers_per_repository][0]) // $default
  ' <<<"$config"
}

# Resolve a roster repository to a primary local checkout. All callers share
# this matcher so sweep diagnostics, dispatch preflight, and the implementer
# cannot disagree about what is usable. A linked worktree is evidence that the
# remote matches, but it is not a safe source checkout: its branch and commits
# belong to another worktree owner. Status 11 distinguishes an unconfigured
# search_roots list from status 1, where roots exist but contain no match.
mandate_find_source_repository() {
  local repository="$1"
  local config="${2:-}"
  local root_count root marker candidate remote normalized
  local -a matching_candidates=()
  local -a primary_candidates=()
  local -a linked_candidates=()

  if [ -z "$config" ]; then
    config="$(mandate_load_config)" || return
  fi

  root_count="$(jq -er '.search_roots | length' <<<"$config")" || return
  [ "$root_count" -gt 0 ] || return 11

  while IFS= read -r root; do
    [ -d "$root" ] || continue
    while IFS= read -r marker; do
      candidate="${marker%/.git}"
      remote="$(git -C "$candidate" remote get-url origin 2>/dev/null)" || continue
      normalized="${remote%.git}"
      normalized="${normalized#https://github.com/}"
      normalized="${normalized#git@github.com:}"
      if [ "$normalized" = "$repository" ]; then
        matching_candidates+=("$candidate")
      fi
    done < <(find "$root" -name .git -print -prune 2>/dev/null)
  done < <(jq -r '.search_roots[]' <<<"$config")

  if [ "${#matching_candidates[@]}" -gt 0 ]; then
    # Sort before classification so overlapping roots and filesystem traversal
    # order cannot change which primary checkout wins.
    while IFS= read -r candidate; do
      if [ -d "$candidate/.git" ]; then
        primary_candidates+=("$candidate")
      elif [ -f "$candidate/.git" ]; then
        linked_candidates+=("$candidate")
      fi
    done < <(printf '%s\n' "${matching_candidates[@]}" | LC_ALL=C sort -u)
  fi

  if [ "${#primary_candidates[@]}" -gt 0 ]; then
    printf '%s\n' "${primary_candidates[0]}"
    return 0
  fi
  if [ "${#linked_candidates[@]}" -gt 0 ]; then
    printf 'source-repository-linked-worktree-only path=%s\n' \
      "${linked_candidates[0]}"
    return 10
  fi
  return 1
}

mandate_gate_is_configured() {
  [ -f "$MANDATE_GATE_USER_CONFIG" ] || [ -f "$MANDATE_GATE_REPO_CONFIG" ]
}

# Parse gate.yaml's deliberately small schema. It mirrors the mandate roster's
# project layering and selector vocabulary, but remains a separate config so
# neither delivery role can change the conditions it is judged by.
mandate_gate_yaml_to_json() {
  gate_file="$1"
  [ -f "$gate_file" ] || {
    printf '{}\n'
    return 0
  }

  awk '
    function trim(s) {
      sub(/^[[:space:]]+/, "", s)
      sub(/[[:space:]]+$/, "", s)
      return s
    }
    function unquote(s, first, last) {
      s = trim(s)
      first = substr(s, 1, 1)
      last = substr(s, length(s), 1)
      if (length(s) >= 2 && ((first == "\"" && last == "\"") || (first == "\047" && last == "\047"))) {
        s = substr(s, 2, length(s) - 2)
      }
      return s
    }
    function fail(message) {
      printf "%s:%d: gate config: %s\n", FILENAME, NR, message > "/dev/stderr"
      failed = 1
    }
    BEGIN { section = ""; current_repo = ""; project_list = ""; failed = 0 }
    {
      raw = $0
      if (raw ~ /\t/) {
        fail("tabs are not supported")
        next
      }
      sub(/[[:space:]]+#.*$/, "", raw)
      if (raw ~ /^[[:space:]]*$/ || raw ~ /^[[:space:]]*#/) next

      match(raw, /^ */)
      indent = RLENGTH
      text = substr(raw, indent + 1)

      if (indent == 0) {
        section = ""
        current_repo = ""
        project_list = ""
        if (text == "bounce_all:") {
          section = "bounce_all"
          print "array\tbounce_all"
        } else if (text == "bounce_all: []") {
          print "array\tbounce_all"
        } else if (text == "projects:") {
          section = "projects"
          print "array\tprojects"
        } else if (text == "projects: []") {
          print "array\tprojects"
        } else if (text ~ /^provider:[[:space:]]*/) {
          value = text
          sub(/^provider:[[:space:]]*/, "", value)
          value = unquote(value)
          if (value == "") fail("empty provider")
          else print "scalar\tprovider\t" value
        } else {
          fail("unsupported root entry: " text)
        }
        next
      }

      if (section == "bounce_all" && indent == 2 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (value == "") fail("empty bounce_all entry")
        else print "bounce_all\t" value
        next
      }

      if (section == "projects" && indent == 2 && text ~ /^-[[:space:]]+repo:[[:space:]]*/) {
        value = text
        sub(/^-[[:space:]]+repo:[[:space:]]*/, "", value)
        current_repo = unquote(value)
        project_list = ""
        if (current_repo == "") fail("empty project repo")
        else print "project\t" current_repo
        next
      }

      if (section == "projects" && current_repo != "" && indent == 4 &&
          (text ~ /^(required_checks|bounce|reserved):$/ ||
           text ~ /^(required_checks|bounce|reserved): \[\]$/)) {
        project_list = text
        sub(/:.*/, "", project_list)
        print "project_array\t" current_repo "\t" project_list
        next
      }
      if (section == "projects" && current_repo != "" && indent == 6 && text ~ /^-[[:space:]]+/) {
        value = text
        sub(/^-[[:space:]]+/, "", value)
        value = unquote(value)
        if (project_list == "") fail("project list entry has no list heading for " current_repo)
        else if (value == "") fail("empty project " project_list " entry")
        else print "project_list\t" current_repo "\t" project_list "\t" value
        next
      }

      fail("unsupported indentation or entry: " text)
    }
    END { if (failed) exit 2 }
  ' "$gate_file" |
    jq -Rn '
      reduce inputs as $line ({};
        ($line | split("\t")) as $parts
        | if $parts[0] == "scalar" then
            .[$parts[1]] = $parts[2]
          elif $parts[0] == "array" then
            .[$parts[1]] = []
          elif $parts[0] == "bounce_all" then
            .bounce_all += [$parts[1]]
          elif $parts[0] == "project" then
            .projects += [{
              "repo": $parts[1],
              "required_checks": [],
              "bounce": [],
              "reserved": []
            }]
          elif $parts[0] == "project_array" then
            (.projects | map(.repo) | index($parts[1])) as $index
            | if $index == null
              then error("project list appeared before its repo")
              else .projects[$index][$parts[2]] = []
              end
          elif $parts[0] == "project_list" then
            (.projects | map(.repo) | index($parts[1])) as $index
            | if $index == null
              then error("project list entry appeared before its repo")
              elif $parts[2] == "reserved"
              then .projects[$index].reserved += [
                ($parts[3] | ltrimstr("#") | tonumber)
              ]
              else .projects[$index][$parts[2]] += [$parts[3]]
              end
          else error("unknown parser record")
          end
      )
    '
}

mandate_load_gate_config() {
  gate_shipped="$(mandate_gate_yaml_to_json "$MANDATE_GATE_DEFAULT_CONFIG")" || return
  gate_user="$(mandate_gate_yaml_to_json "$MANDATE_GATE_USER_CONFIG")" || return
  gate_repo="$(mandate_gate_yaml_to_json "$MANDATE_GATE_REPO_CONFIG")" || return

  gate_config="$(
    jq -cn \
      --argjson shipped "$gate_shipped" \
      --argjson user "$gate_user" \
      --argjson repo "$gate_repo" \
      '$shipped * $user * $repo'
  )" || return

  if ! jq -e '
    .provider == "file"
    and (.bounce_all | type == "array" and all(.[]; type == "string" and length > 0))
    and (.projects | type == "array")
    and all(.projects[];
      (.repo | type == "string" and test("^[^/[:space:]]+/[^/[:space:]]+$"))
      and (.required_checks | type == "array" and all(.[]; type == "string" and length > 0))
      and (.bounce | type == "array" and all(.[]; type == "string" and length > 0))
      and (.reserved | type == "array" and all(.[]; type == "number" and . > 0 and . == floor))
    )
    and (([.projects[].repo] | length) == ([.projects[].repo] | unique | length))
  ' >/dev/null <<<"$gate_config"; then
    echo "mandate gate: invalid config; provider must be file, every project must have a unique owner/name repo, required-check and bounce selectors must be non-empty strings, and reserved refs must be positive integers" >&2
    return 2
  fi

  gate_selector_error="$(
    jq -r '
      def selector_records:
        (.bounce_all[]? | {where: "bounce_all", selector: .}),
        (.projects[]? as $project
          | ($project.bounce[]? | {
              where: ($project.repo + " bounce"), selector: .
            })
        );
      def lint:
        .selector as $selector
        | ((try ($selector | capture("^(?<prefix>[^:]+):(?<glob>.*)$")) catch null) // null) as $parsed
        | if $parsed == null or
             ($parsed.prefix | IN("label", "scope", "type", "path", "ref", "title") | not)
          then "unknown selector prefix"
          elif $parsed.glob == ""
          then "selector value is empty"
          elif $parsed.prefix == "ref" and
               ($parsed.glob | test("^#[1-9][0-9]*$") | not)
          then "ref selector must be ref:#N"
          elif $parsed.prefix == "title" and
               ($parsed.glob | contains("*") | not)
          then "title selector must contain *"
          elif $parsed.prefix == "title" and
               (($parsed.glob | split("*") | map(length) | max) > 24)
          then "title selector literal run exceeds 24 characters"
          else empty
          end;
      first(selector_records as $record
        | ($record | lint) as $message
        | "\($record.where) selector \"\($record.selector)\": \($message)"
      ) // empty
    ' <<<"$gate_config"
  )" || return
  if [ -n "$gate_selector_error" ]; then
    echo "mandate gate: invalid config: $gate_selector_error" >&2
    return 2
  fi

  printf '%s\n' "$gate_config"
}

mandate_read_queue() {
  if [ ! -s "$MANDATE_QUEUE_FILE" ]; then
    printf '[]\n'
    return 0
  fi

  jq -s '
    if all(.[];
      (
        (["id","kind","mandate","opened","ref","repo","state"] - keys | length) == 0
        and
        (keys - [
          "age_days", "aged_out", "blocked_by", "classification", "id", "kind",
          "mandate", "matched_selector", "needs_judgment", "opened", "ref",
          "repo", "semantic_derivation", "state", "title"
        ] | length) == 0
      )
      and (.id | type == "string")
      and (.repo | type == "string")
      and (.kind | IN("tripwire", "decision", "moved", "stuck", "drift", "parked", "merge-gate-fault", "unexplained-write"))
      and (
        (.ref | type == "string")
        and (
          (.ref | test("^#[0-9]+$"))
          or (
            .kind == "unexplained-write"
            and (.ref | test("^@[^[:cntrl:][:space:]]+$"))
          )
        )
      )
      and (.state | IN("pending", "approved", "deferred"))
      and (.opened | type == "string")
      and (
        (has("age_days") | not)
        or (.age_days | type == "number" and . >= 0 and . == floor)
      )
      and ((has("aged_out") | not) or (.aged_out | type == "boolean"))
      and (
        (has("needs_judgment") | not)
        or (.needs_judgment | type == "boolean")
      )
      and (
        (has("blocked_by") | not)
        or (
          .blocked_by | type == "array"
          and all(.[];
            type == "string"
            and test("^[^/[:space:]#]+/[^/[:space:]#]+#[1-9][0-9]*$")
          )
        )
      )
      and (
        (has("title") | not)
        or (((.title | type) == "string") and ((.title | length) > 0))
      )
      and (
        (has("semantic_derivation") | not)
        and (has("classification") | not)
        and (has("matched_selector") | not)
        or (
          has("semantic_derivation")
          and has("classification")
          and has("matched_selector")
          and (.classification | IN("delegated", "excluded", "unclassified", "reserved", "tripwire"))
          and (.matched_selector | type == "string" and length > 0)
          and (.semantic_derivation | type == "object")
          and (.semantic_derivation.findings | type == "array")
          and all(.semantic_derivation.findings[];
            (.kind | IN("parked", "already_decided", "genuinely_stuck", "actually_a_release"))
            and (.confidence | type == "number" and . >= 0 and . <= 1)
            and (.evidence | type == "object")
            and (.evidence.source | IN("title", "label", "body", "comment"))
            and (.evidence.quote | type == "string" and length > 0)
          )
          and (
            .semantic_derivation.authority == null
            or (
              (.semantic_derivation.authority.classification | IN("unclassified", "reserved", "tripwire"))
              and (.semantic_derivation.authority.confidence | type == "number" and . >= 0 and . <= 1)
              and (.semantic_derivation.authority.evidence.source | IN("title", "label", "body", "comment"))
              and (.semantic_derivation.authority.evidence.quote | type == "string" and length > 0)
            )
          )
        )
      )
    )
    then .
    else error("queue contains a malformed row")
    end
  ' "$MANDATE_QUEUE_FILE"
}

mandate_write_if_changed() {
  source_file="$1"
  destination="$2"
  if [ -f "$destination" ] && cmp -s "$source_file" "$destination"; then
    return 0
  fi
  mv "$source_file" "$destination"
}

# Append one line to the selector-events log: a queue decision plus the
# selector and classification that produced the row it was decided about.
# The lookup is read-only against the last sweep's state; the append is the
# only write. An item absent from state (state.json missing, or the row
# predates any sweep that classified it) still gets a row with null fields
# instead of silently vanishing — the point is that every decision leaves a
# trace, not that every trace is fully attributed.
mandate_log_selector_event() {
  event_id="$1"
  event_repo="$2"
  event_decision="$3"

  lookup='{"matched_selector":null,"classification":null}'
  if [ -s "$MANDATE_STATE_FILE" ]; then
    lookup="$(
      jq -c --arg repo "$event_repo" --arg id "$event_id" '
        (.repos[$repo].items[$id] // {}) as $item
        | {
            matched_selector: ($item.matched_selector // null),
            classification: ($item.classification // null)
          }
      ' "$MANDATE_STATE_FILE" 2>/dev/null
    )" || lookup='{"matched_selector":null,"classification":null}'
  fi

  mkdir -p "$MANDATE_DATA_DIR"
  # Build the line in a variable and append it with one printf call rather
  # than redirecting jq's stdout with >>: jq is not guaranteed to write its
  # output in a single write(2), so two concurrent /ostrom:desk actions could
  # interleave partial lines and corrupt the JSONL. A single printf is one
  # write and, being under PIPE_BUF, is atomic under O_APPEND — this only
  # narrows the race, it does not close it (a write at or above PIPE_BUF, or
  # a non-POSIX filesystem, can still interleave); a real guarantee needs
  # file locking, deliberately not added here.
  event_line="$(
    jq -cn \
      --arg ts "${MANDATE_EVENT_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}" \
      --arg id "$event_id" \
      --arg decision "$event_decision" \
      --argjson lookup "$lookup" '
        {
          ts: $ts,
          id: $id,
          decision: $decision,
          matched_selector: $lookup.matched_selector,
          classification: $lookup.classification
        }
      '
  )"
  printf '%s\n' "$event_line" >>"$MANDATE_EVENTS_FILE"
}

# A headless Bash tool can refuse to statically permit `source "$path"` at
# all, because sourcing evaluates its argument as shell code and there is no
# safe way to allow-list that ahead of time. Every other caller of this file
# (sweep.sh, audit.sh, replay.sh, local-drift.sh, publish.sh, ...) sources it
# from inside a script it already trusts, so this dispatch must not change
# their behavior. It only runs when the file itself is the thing being
# executed, giving a skill that cannot source it a plain command instead:
# `bash mandate-lib.sh config` prints the same resolved roster JSON
# `mandate_load_config` returns to any in-process caller.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  case "${1:-}" in
    config)
      mandate_load_config
      ;;
    *)
      echo "mandate-lib: usage: $0 config" >&2
      exit 2
      ;;
  esac
fi
