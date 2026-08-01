#!/usr/bin/env bash
# Shared config and queue helpers for the mandate plugin.

MANDATE_PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
MANDATE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
MANDATE_DATA_DIR="$MANDATE_CONFIG_DIR/ostrom"
MANDATE_USER_CONFIG="$MANDATE_DATA_DIR/mandates.yaml"
MANDATE_REPO_CONFIG="./.ostrom/mandates.yaml"
MANDATE_DEFAULT_CONFIG="$MANDATE_PLUGIN_ROOT/config/defaults.yaml"
MANDATE_QUEUE_FILE="$MANDATE_DATA_DIR/queue.jsonl"
MANDATE_STATE_FILE="$MANDATE_DATA_DIR/state.json"

mandate_is_configured() {
  [ -f "$MANDATE_USER_CONFIG" ] || [ -f "$MANDATE_REPO_CONFIG" ]
}

# Parse the deliberately small shipped schema without pretending to be a
# general YAML parser. Supported input:
#   root scalars; bounce_all: followed by two-space list items; projects:
#   followed by "- repo:" entries, each with default + paused scalars and
#   six-space delegated/excluded/reserved/bounce lists.
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
      '$shipped * $user * $repo'
  )" || return

  if ! jq -e '
    .provider == "file"
    and (.cadence_hours | type == "number" and . > 0 and . == floor)
    and (.stuck_after_days | type == "number" and . >= 0)
    and (.bounce_all | type == "array" and all(.[]; type == "string" and length > 0))
    and (.projects | type == "array")
    and all(.projects[];
      (.repo | type == "string" and test("^[^/[:space:]]+/[^/[:space:]]+$"))
      and (.paused | type == "boolean")
      and (.default | IN("delegated", "excluded", "unclassified"))
      and (.delegated | type == "array" and all(.[]; type == "string" and length > 0))
      and (.excluded | type == "array" and all(.[]; type == "string" and length > 0))
      and (.reserved | type == "array" and all(.[]; type == "number" and . > 0 and . == floor))
      and (.bounce | type == "array" and all(.[]; type == "string" and length > 0))
    )
    and (([.projects[].repo] | length) == ([.projects[].repo] | unique | length))
  ' >/dev/null <<<"$config"; then
    echo "mandate: invalid config; provider must be file, cadence_hours a positive integer, and every project must have a unique owner/name repo, valid default, boolean paused value, selector lists, and positive integer reserved refs" >&2
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
          "age_days", "blocked_by", "id", "kind", "mandate",
          "needs_judgment", "opened", "ref", "repo", "state", "stuck", "title"
        ] | length) == 0
      )
      and (.id | type == "string")
      and (.repo | type == "string")
      and (.ref | type == "string" and test("^#[0-9]+$"))
      and (.kind | IN("tripwire", "decision", "moved", "stuck", "drift"))
      and (.state | IN("pending", "approved", "deferred"))
      and (.opened | type == "string")
      and (
        (has("age_days") | not)
        or (.age_days | type == "number" and . >= 0 and . == floor)
      )
      and ((has("stuck") | not) or (.stuck | type == "boolean"))
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
