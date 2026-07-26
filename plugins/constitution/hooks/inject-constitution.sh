#!/usr/bin/env bash
# inject-constitution.sh — SessionStart hook: emit the layered constitution.
#
# Concatenates rule layers most-specific-LAST, so a later layer wins on
# conflict:
#   1. rules/frozen-rules.md                                — shipped (always present)
#   2. ~/.claude/ostrom/rules.md,
#      then ~/.claude/ostrom/rules.d/*.md (sorted)           — user (machine-local)
#   3. ./.ostrom/rules.md,
#      then ./.ostrom/rules.d/*.md (sorted)                  — repo (project-local)
#
# Mirrors the touch-log config's layering (defaults -> user -> repo), minus
# the org `extends:` hop — rules have no fetch story yet.
#
# A missing optional layer is skipped silently — a fresh install with no
# user/repo layer sees output byte-identical to a bare `cat frozen-rules.md`.
# Each layer that actually fires is preceded by an HTML-comment provenance
# marker naming the file it came from, so a reader can tell which
# constitution a rule belongs to.
#
# A layer file that is nothing but HTML comments and blank lines (e.g. a
# freshly-seeded template, as a private user-layer repo typically ships
# before any real rule is written) contributes NOTHING: no marker, no
# content, and it does NOT count toward "at least one extra layer fired" —
# so the override note stays suppressed too. This keeps the SessionStart
# budget clean until a layer actually carries a rule. Once a file DOES
# qualify, it is emitted verbatim, comments included — the
# Source/Preconditions provenance comments are load-bearing content, not
# noise to strip from the output itself.
#
# set -u (not set -e): a missing optional layer must never abort the hook —
# a SessionStart hook that fails is worse than one that injects nothing. This
# script never writes to stderr or exits non-zero on the ordinary path.

set -u

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-}"
CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

shopt -s nullglob   # an unmatched rules.d/*.md glob expands to nothing, never a literal path

shipped="$PLUGIN_ROOT/rules/frozen-rules.md"

# has_content <file> — true (exit 0) iff, after stripping HTML comment
# blocks (including ones spanning multiple lines) and blank lines, anything
# non-whitespace remains. Pure awk, no perl: walks each line character-range
# by character-range, tracking whether we're inside a `<!-- ... -->` block
# across line boundaries, and flags the first non-comment/non-blank text it
# finds.
has_content() {
  awk '
    BEGIN { in_comment = 0; found = 0 }
    {
      line = $0
      while (length(line) > 0) {
        if (in_comment) {
          p = index(line, "-->")
          if (p == 0) { line = "" } else { line = substr(line, p + 3); in_comment = 0 }
        } else {
          p = index(line, "<!--")
          if (p == 0) {
            seg = line; line = ""
          } else {
            seg = substr(line, 1, p - 1); line = substr(line, p + 4); in_comment = 1
          }
          gsub(/[ \t]/, "", seg)
          if (length(seg) > 0) { found = 1 }
        }
      }
    }
    END { exit(found ? 0 : 1) }
  ' "$1" 2>/dev/null
}

# Collect every extra (non-shipped) file that actually fires, in emission
# order: user rules.md, user rules.d/*.md (sorted), repo rules.md, repo
# rules.d/*.md (sorted). A candidate only joins the array if has_content
# says it carries real text outside of HTML comments.
layer_labels=()
layer_files=()

user_dir="$CONFIG_DIR/ostrom"
if [ -f "$user_dir/rules.md" ] && has_content "$user_dir/rules.md"; then
  layer_labels+=("user")
  layer_files+=("$user_dir/rules.md")
fi
for f in "$user_dir/rules.d/"*.md; do
  has_content "$f" || continue
  layer_labels+=("user")
  layer_files+=("$f")
done

repo_dir="./.ostrom"
if [ -f "$repo_dir/rules.md" ] && has_content "$repo_dir/rules.md"; then
  layer_labels+=("repo")
  layer_files+=("$repo_dir/rules.md")
fi
for f in "$repo_dir/rules.d/"*.md; do
  has_content "$f" || continue
  layer_labels+=("repo")
  layer_files+=("$f")
done

# 1. Shipped layer — always, no marker, no note. This keeps a plain
#    install's output byte-identical to today's bare `cat`.
[ -f "$shipped" ] && cat "$shipped"

# 2. Extra layers — only when at least one fired, so the override note
#    never appears on the default "no extra layers" path.
if [ "${#layer_files[@]}" -gt 0 ]; then
  echo
  echo "<!-- constitution: layers below override the shipped rules above on conflict -->"
  for i in "${!layer_files[@]}"; do
    file="${layer_files[$i]}"
    label="${layer_labels[$i]}"
    display="${file/#$HOME/~}"
    echo
    echo "<!-- constitution layer: ${label} (${display}) -->"
    echo
    cat "$file"
  done
fi

exit 0
