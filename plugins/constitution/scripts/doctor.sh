#!/usr/bin/env bash
# doctor.sh — read-only prober for /doctor: make silent degradation visible.
#
# A half-onboarded machine does not fail — it degrades silently. The
# SessionStart hook injects the shipped rules and nothing else, and /touch
# falls back to the `file` provider and appends to a local markdown file
# indefinitely. Both look exactly like working. This script is the thing
# that actually checks.
#
# THIS SCRIPT PERFORMS EXACTLY ONE WRITE: check 2 runs `git fetch` into the
# marketplace's own cached clone, which updates that clone's remote-tracking
# refs and objects only — it never moves HEAD, never touches a working tree,
# and never writes any file the user owns. That write is what makes the
# unrelated-histories diagnosis possible, so it stays; everything else in
# this script is read-only — no config file is created or modified, nothing
# is installed, the touch log is never touched.
#
# Output: one line per check on stdout —
#
#   STATUS|check-name|detail|remedy
#
# STATUS is OK, WARN, FAIL, or DEFER. remedy is empty on OK. Fields are
# sanitized to never contain a literal `|` (see emit()).
#
# DEFER means the shell genuinely cannot answer the question — right now
# only check 5's notion case, where MCP availability is a property of the
# running Claude session, not of anything a subprocess can observe. A DEFER
# line is a handoff: the calling skill (running inside that session) must
# resolve it from its own tool availability and must never show a raw DEFER
# to the user.
#
# A failing CHECK is data, not a script error: this script always exits 0.
# The only thing that would make it exit non-zero is a bug in the script
# itself, which is why it's written defensively (set -u, every subprocess's
# stderr captured rather than left to leak, every path guarded with -f/-d
# before use — CLAUDE_CONFIG_DIR pointing at a directory that doesn't exist
# at all is a supported, silent-no-op input, not an error).
#
# set -u (not set -e): the whole point of this script is to keep going
# through failing checks and report all seven, not stop at the first one.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

# emit STATUS name detail remedy — the one place that writes a result line.
# Strips `|` and newlines out of the free-text fields so the pipe-delimited
# format can never be corrupted by what a check happened to read.
emit() {
  local status="$1" name="$2" detail="$3" remedy="$4"
  detail="$(printf '%s' "$detail" | tr '\n' ' ')"
  remedy="$(printf '%s' "$remedy" | tr '\n' ' ')"
  detail="${detail//|/\/}"
  remedy="${remedy//|/\/}"
  printf '%s|%s|%s|%s\n' "$status" "$name" "$detail" "$remedy"
}

# expand_tilde PATH — the touch config stores paths with a literal leading
# `~`, meaning the real $HOME regardless of CLAUDE_CONFIG_DIR (this is a
# convention of the config format, not shell tilde expansion, since the
# value comes out of a YAML string).
expand_tilde() {
  case "$1" in
    "~") printf '%s' "$HOME" ;;
    "~/"*) printf '%s' "$HOME/${1#\~/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# ---------------------------------------------------------------------------
# 1. plugin — is constitution installed, and what version.
# ---------------------------------------------------------------------------
check_plugin() {
  local installed_json="$CONFIG_DIR/plugins/installed_plugins.json"
  if [ ! -f "$installed_json" ]; then
    emit FAIL plugin "no installed_plugins.json at $installed_json" "/plugin install constitution@ostrom"
    return
  fi
  # Claude Code's own fixed JSON shape — a marker-then-grep extraction, not a
  # general JSON parser. See check 7 for the honesty note about YAML; this
  # JSON shape is simple and stable enough that grep is the right tool, not
  # a corner cut.
  local block install_path recorded_version
  block="$(awk '/"constitution@ostrom"/{f=1} f' "$installed_json" 2>/dev/null)"
  if [ -z "$block" ]; then
    emit FAIL plugin "constitution@ostrom not present in installed_plugins.json" "/plugin install constitution@ostrom"
    return
  fi
  install_path="$(printf '%s\n' "$block" | grep -m1 '"installPath"' | sed -E 's/.*"installPath"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')"
  recorded_version="$(printf '%s\n' "$block" | grep -m1 '"version"' | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')"
  if [ -n "$install_path" ] && [ -f "$install_path/.claude-plugin/plugin.json" ]; then
    local pj_version
    pj_version="$(grep -m1 '"version"' "$install_path/.claude-plugin/plugin.json" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')"
    emit OK plugin "installed, version $pj_version" ""
  elif [ -n "$recorded_version" ]; then
    emit OK plugin "installed, version $recorded_version (installPath plugin.json not readable, using registry-recorded version)" ""
  else
    emit FAIL plugin "constitution@ostrom entry found but no version could be determined" "/plugin install constitution@ostrom"
  fi
}

# ---------------------------------------------------------------------------
# 2. marketplace — registered, and can the cached clone fast-forward.
# ---------------------------------------------------------------------------
check_marketplace() {
  local known_json="$CONFIG_DIR/plugins/known_marketplaces.json"
  local mp_dir="$CONFIG_DIR/plugins/marketplaces/ostrom"

  if [ ! -f "$known_json" ] || ! grep -q '"ostrom"[[:space:]]*:' "$known_json" 2>/dev/null; then
    emit FAIL marketplace "ostrom not registered in known_marketplaces.json" "/plugin marketplace add onsager-ai/ostrom"
    return
  fi
  if [ ! -d "$mp_dir/.git" ]; then
    emit FAIL marketplace "registered, but no cached clone at $mp_dir" "/plugin marketplace add onsager-ai/ostrom"
    return
  fi

  local fetch_out fetch_rc
  fetch_out="$(git -C "$mp_dir" fetch origin main 2>&1)"; fetch_rc=$?
  if [ "$fetch_rc" -ne 0 ]; then
    # Offline is an expected, non-broken state — WARN, never FAIL, so a
    # disconnected laptop doesn't read as a wiring problem.
    emit WARN marketplace "cannot verify freshness, git fetch failed (offline?): $(printf '%s' "$fetch_out" | head -1)" ""
    return
  fi
  if ! git -C "$mp_dir" rev-parse --verify origin/main >/dev/null 2>&1; then
    emit WARN marketplace "fetched, but origin/main not found (default branch may differ)" ""
    return
  fi
  if git -C "$mp_dir" merge-base --is-ancestor HEAD origin/main 2>/dev/null; then
    emit OK marketplace "cached clone can fast-forward to origin/main" ""
    return
  fi
  # HEAD is not an ancestor of origin/main. Distinguish "diverged but shares
  # history" (plain merge-base still finds a common ancestor) from "unrelated
  # histories" (it finds nothing) — the latter is exactly what republishing
  # this repo from a fresh git history does to every existing cached clone,
  # and it needs its own diagnosis, not a generic git error.
  if git -C "$mp_dir" merge-base HEAD origin/main >/dev/null 2>&1; then
    emit WARN marketplace "cached clone has diverged from origin/main (shared history, not fast-forwardable)" "/plugin marketplace update ostrom"
  else
    emit FAIL marketplace "cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)" "/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom"
  fi
}

# ---------------------------------------------------------------------------
# Shared: run the real hook and report which layers fired. Every check that
# needs this (3 and 6) calls it fresh rather than reading a global another
# check happened to set first — read-only and cheap, so there is no reason
# any check here should depend on another having already run. Same rule
# resolve_touch_config follows below for checks 4/5.
# ---------------------------------------------------------------------------
compute_rules_layers() {
  # Sets: LAYERS_HOOK_MISSING, LAYERS_HAS_USER, LAYERS_HAS_REPO
  local shipped_hook="$PLUGIN_ROOT/hooks/inject-constitution.sh"
  LAYERS_HOOK_MISSING=0
  LAYERS_HAS_USER=0
  LAYERS_HAS_REPO=0
  if [ ! -f "$shipped_hook" ]; then
    LAYERS_HOOK_MISSING=1
    return
  fi
  local hook_out
  hook_out="$(CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" CLAUDE_CONFIG_DIR="$CONFIG_DIR" bash "$shipped_hook" 2>/dev/null)"
  printf '%s\n' "$hook_out" | grep -q '<!-- constitution layer: user ' && LAYERS_HAS_USER=1
  printf '%s\n' "$hook_out" | grep -q '<!-- constitution layer: repo ' && LAYERS_HAS_REPO=1
}

# ---------------------------------------------------------------------------
# 3. rules-layers — run the real hook, see which layers actually fired.
# ---------------------------------------------------------------------------
check_rules_layers() {
  compute_rules_layers
  if [ "$LAYERS_HOOK_MISSING" -eq 1 ]; then
    emit FAIL rules-layers "hook not found at $PLUGIN_ROOT/hooks/inject-constitution.sh" "reinstall the constitution plugin"
    return
  fi

  local summary="shipped"
  [ "$LAYERS_HAS_USER" -eq 1 ] && summary="$summary + user"
  [ "$LAYERS_HAS_REPO" -eq 1 ] && summary="$summary + repo"
  [ "$summary" = "shipped" ] && summary="shipped only"

  # A user/repo rules.md that EXISTS but is comment-only contributes nothing
  # BY DESIGN (the hook's own has_content check skips it) — that must read
  # as OK-with-detail, never as a fault.
  local notes=""
  if [ -f "$CONFIG_DIR/ostrom/rules.md" ] && [ "$LAYERS_HAS_USER" -eq 0 ]; then
    notes="user layer present but carries no rules yet (by design)"
  fi
  if [ -f "./.ostrom/rules.md" ] && [ "$LAYERS_HAS_REPO" -eq 0 ]; then
    if [ -n "$notes" ]; then
      notes="$notes; repo layer present but carries no rules yet (by design)"
    else
      notes="repo layer present but carries no rules yet (by design)"
    fi
  fi

  if [ -n "$notes" ]; then
    emit OK rules-layers "$summary ($notes)" ""
  else
    emit OK rules-layers "$summary" ""
  fi
}

# ---------------------------------------------------------------------------
# Shared: resolve the layered touch config the way skills/touch/SKILL.md
# does — shipped defaults.yaml < user config.yaml < repo config.yaml.
# (The org `extends:` hop is a network fetch skills/touch performs at use
# time; doctor stays local-only, same as every other check but #2's git
# fetch, so it does not follow extends.)
# ---------------------------------------------------------------------------
PARSER_NAME=""
RESOLVED_PROVIDER=""
RESOLVED_PATH_RAW=""
RESOLVED_AUTO_COMMIT=""

resolve_touch_config() {
  local shipped="$PLUGIN_ROOT/config/defaults.yaml"
  local user_cfg="$CONFIG_DIR/ostrom/config.yaml"
  local repo_cfg="./.ostrom/config.yaml"

  # Prefer a real YAML parser when one is available, so checks 4/5 are
  # authoritative rather than a heuristic guess. Only python3+PyYAML is
  # tried — no assumption that yq is installed.
  if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' >/dev/null 2>&1; then
    local out
    out="$(python3 - "$shipped" "$user_cfg" "$repo_cfg" <<'PY' 2>/dev/null
import sys, yaml
def load(p):
    try:
        with open(p) as fh:
            d = yaml.safe_load(fh)
            return d if isinstance(d, dict) else {}
    except Exception:
        return {}
merged = {}
for p in sys.argv[1:]:
    d = load(p)
    for k, v in d.items():
        if isinstance(v, dict) and isinstance(merged.get(k), dict):
            merged[k].update(v)
        else:
            merged[k] = v
provider = merged.get('provider', 'file')
fb = merged.get('file') if isinstance(merged.get('file'), dict) else {}
print("provider=%s" % provider)
print("path=%s" % fb.get('path', ''))
print("auto_commit=%s" % fb.get('auto_commit', False))
PY
)"
    RESOLVED_PROVIDER="$(printf '%s\n' "$out" | sed -n 's/^provider=//p' | head -1)"
    RESOLVED_PATH_RAW="$(printf '%s\n' "$out" | sed -n 's/^path=//p' | head -1)"
    RESOLVED_AUTO_COMMIT="$(printf '%s\n' "$out" | sed -n 's/^auto_commit=//p' | head -1)"
    if [ -n "$RESOLVED_PROVIDER" ]; then
      PARSER_NAME="python3+PyYAML"
    fi
  fi

  if [ -z "$PARSER_NAME" ]; then
    # Heuristic fallback: a line-scanner limited to top-level scalars and one
    # level of nesting (the `file:` block). No anchors, no multi-line
    # strings, no lists — this is NOT a YAML parser, it is exactly enough to
    # read the shape ostrom's own config actually ships. Check 7 says so.
    RESOLVED_PROVIDER="file"
    RESOLVED_PATH_RAW="~/.claude/ostrom/touch-log.md"
    RESOLVED_AUTO_COMMIT="false"
    local f v
    for f in "$shipped" "$user_cfg" "$repo_cfg"; do
      [ -f "$f" ] || continue
      v="$(grep -E '^provider:' "$f" 2>/dev/null | tail -1 | sed -E 's/^provider:[[:space:]]*//; s/[[:space:]]*#.*$//; s/^"(.*)"$/\1/')"
      [ -n "$v" ] && RESOLVED_PROVIDER="$v"
      if grep -qE '^file:[[:space:]]*$' "$f" 2>/dev/null; then
        v="$(awk '/^file:[[:space:]]*$/{f=1;next} /^[^[:space:]]/{f=0} f && /path:/{print; exit}' "$f" | sed -E 's/^[[:space:]]*path:[[:space:]]*//; s/[[:space:]]*#.*$//')"
        [ -n "$v" ] && RESOLVED_PATH_RAW="$v"
        v="$(awk '/^file:[[:space:]]*$/{f=1;next} /^[^[:space:]]/{f=0} f && /auto_commit:/{print; exit}' "$f" | sed -E 's/^[[:space:]]*auto_commit:[[:space:]]*//; s/[[:space:]]*#.*$//')"
        [ -n "$v" ] && RESOLVED_AUTO_COMMIT="$v"
      fi
    done
  fi

  [ -n "$RESOLVED_PATH_RAW" ] || RESOLVED_PATH_RAW="~/.claude/ostrom/touch-log.md"
  [ -n "$RESOLVED_PROVIDER" ] || RESOLVED_PROVIDER="file"
}

# ---------------------------------------------------------------------------
# 4. touch-durability — the check that matters most.
# ---------------------------------------------------------------------------
check_touch_durability() {
  resolve_touch_config
  local expanded_path
  expanded_path="$(expand_tilde "$RESOLVED_PATH_RAW")"

  local target_status target_detail target_remedy
  case "$RESOLVED_PROVIDER" in
    notion)
      target_status="OK"
      target_detail="provider notion (target is inherently shared)"
      target_remedy=""
      ;;
    file)
      local dir
      dir="$(dirname -- "$expanded_path")"
      if git -C "$dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        target_status="OK"
        target_detail="file provider, $expanded_path is inside a git repo (auto_commit=$RESOLVED_AUTO_COMMIT)"
        target_remedy=""
      else
        target_status="WARN"
        target_detail="file provider, $expanded_path is NOT inside a git repo — touches logged here never reach another machine"
        target_remedy="point file.path into a synced repo and set auto_commit: true, or switch provider"
      fi
      ;;
    *)
      target_status="WARN"
      target_detail="unknown provider '$RESOLVED_PROVIDER' (durability undetermined)"
      target_remedy="check the resolved touch config's provider value"
      ;;
  esac

  # Is the RESOLVED user config file itself versioned? A symlink into a git
  # repo syncs across machines; a plain file is machine-local and silently
  # stays that way forever. Orthogonal to the provider check above — applies
  # whenever a user config.yaml exists, regardless of what it configures.
  local user_cfg="$CONFIG_DIR/ostrom/config.yaml"
  local cfg_status cfg_detail cfg_remedy
  if [ -L "$user_cfg" ]; then
    local link_target link_dir
    link_target="$(readlink -f -- "$user_cfg" 2>/dev/null)"
    link_dir="$(dirname -- "${link_target:-$user_cfg}")"
    if [ -n "$link_target" ] && git -C "$link_dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      cfg_status="OK"
      cfg_detail="config.yaml is a symlink into a git repo (versioned, syncs across machines)"
      cfg_remedy=""
    else
      cfg_status="WARN"
      cfg_detail="config.yaml is a symlink, but its target is not inside a git repo"
      cfg_remedy="version the symlink target in a private config repo"
    fi
  elif [ -f "$user_cfg" ]; then
    cfg_status="WARN"
    cfg_detail="config.yaml is a plain machine-local file (will not sync across machines)"
    cfg_remedy="version it: move it into a private config repo and symlink it back to $user_cfg"
  else
    cfg_status="OK"
    cfg_detail="no user config.yaml present (shipped defaults only)"
    cfg_remedy=""
  fi

  local overall="OK"
  { [ "$target_status" = "WARN" ] || [ "$cfg_status" = "WARN" ]; } && overall="WARN"

  local remedy=""
  [ -n "$target_remedy" ] && remedy="$target_remedy"
  if [ -n "$cfg_remedy" ]; then
    if [ -n "$remedy" ]; then remedy="$remedy; $cfg_remedy"; else remedy="$cfg_remedy"; fi
  fi

  emit "$overall" touch-durability "target: $target_detail -- config: $cfg_detail" "$remedy"
}

# ---------------------------------------------------------------------------
# 5. provider-reachable — best-effort, never guess OK.
# ---------------------------------------------------------------------------
check_provider_reachable() {
  # resolve_touch_config was already called by check_touch_durability, but
  # doctor's checks must not depend on call order, so resolve again — it's
  # read-only and cheap.
  resolve_touch_config
  local expanded_path
  expanded_path="$(expand_tilde "$RESOLVED_PATH_RAW")"

  case "$RESOLVED_PROVIDER" in
    notion)
      # Deliberately NOT shelling out to `claude mcp list`: MCP availability
      # is a property of the running Claude session (which connectors are
      # live for ITS tools), not of what a nested CLI invocation reports —
      # the two can disagree in both directions, and a confidently wrong
      # answer is worse than an honest DEFER. The calling skill runs inside
      # that session and can see its own tool availability directly; this
      # shell cannot, so it hands the question back rather than guessing.
      emit DEFER provider-reachable "notion: MCP availability is a session property, not visible to a shell" ""
      ;;
    file)
      local dir existing_dir
      dir="$(dirname -- "$expanded_path")"
      existing_dir="$dir"
      while [ ! -d "$existing_dir" ] && [ "$existing_dir" != "/" ] && [ -n "$existing_dir" ]; do
        existing_dir="$(dirname -- "$existing_dir")"
      done
      if [ -w "$existing_dir" ]; then
        if [ -d "$dir" ]; then
          emit OK provider-reachable "file: $dir is writable" ""
        else
          emit OK provider-reachable "file: $dir does not exist yet, nearest existing ancestor $existing_dir is writable" ""
        fi
      else
        emit FAIL provider-reachable "file: $existing_dir is not writable — /touch cannot write its log" "fix permissions on $existing_dir, or point file.path elsewhere"
      fi
      ;;
    *)
      emit WARN provider-reachable "unknown provider '$RESOLVED_PROVIDER' (undetermined)" ""
      ;;
  esac
}

# ---------------------------------------------------------------------------
# 6. environment — local vs cloud, and whether that combination is expected.
# ---------------------------------------------------------------------------
check_environment() {
  if [ -z "${CLAUDE_CODE_REMOTE:-}" ]; then
    emit OK environment "local" ""
    return
  fi
  # Cloud session: resolve the user-rules-layer question independently of
  # check 3 (compute_rules_layers is read-only and cheap — see the comment
  # above it), so this check gives the same answer whether or not check 3
  # ran first.
  compute_rules_layers
  if [ "$LAYERS_HAS_USER" -eq 1 ]; then
    emit OK environment "cloud, user rules layer resolved" ""
  else
    emit WARN environment "cloud session, no user rules layer resolved (private layer absent)" "provide the private layer's credentials/config for this environment"
  fi
}

# ---------------------------------------------------------------------------
# 7. config-parser — honesty about what checks 4/5 actually did.
# ---------------------------------------------------------------------------
check_config_parser() {
  # resolve_touch_config has already run (checks 4/5) and set PARSER_NAME.
  if [ -n "$PARSER_NAME" ]; then
    emit OK config-parser "used $PARSER_NAME to parse the layered touch config (the values behind touch-durability/provider-reachable are authoritative; a DEFER line is still resolved by the caller)" ""
  else
    emit WARN config-parser "no YAML parser available (tried python3+PyYAML); fell back to a line-scanner limited to top-level scalars and one level of nesting — touch-durability/provider-reachable are heuristic, not authoritative" "install PyYAML for python3 (pip install pyyaml) for authoritative parsing"
  fi
}

# ---------------------------------------------------------------------------
# Run all seven, in order. Nothing above this point has side effects; this
# is simply the order the report reads in.
# ---------------------------------------------------------------------------
check_plugin
check_marketplace
check_rules_layers
check_touch_durability
check_provider_reachable
check_environment
check_config_parser

exit 0
