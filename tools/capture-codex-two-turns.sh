#!/usr/bin/env bash
# Two-turn codex capture for onsager-ai/umwelt#20.
#
# Settles onsager-ai/ethogram#6's stated precondition: does codex report SESSION-CUMULATIVE
# usage on turn.completed, or a PER-TURN increment? A codex normaliser cannot be
# written until this is known, because a cumulative wire requires the normaliser
# to accumulate before emitting if the harness reports increments.
#
# ONE authorised run. Do not retry on a usage-limit error — stop and report.
# Codex usage limit is exhausted until 2026-09-15 09:47.

set -euo pipefail

OUT="${1:?usage: capture-codex-two-turns.sh <output-directory>}"
[ -e "$OUT" ] && { echo "refusing to overwrite existing $OUT" >&2; exit 1; }
mkdir -p "$OUT"
WS="$OUT/workspace"; mkdir -p "$WS"

# The discriminator. input_tokens grows every turn regardless, because the prompt
# carries the history — so input_tokens proves nothing. output_tokens is the test:
# turn 1 is asked for a LARGE reply, turn 2 for a TINY one. If turn 2's
# output_tokens is small, usage is per-turn. If it is >= turn 1's, it is cumulative.
PROMPT_1='Run `seq 1 60` with the shell, then write every number it printed back in your reply, space-separated, on one line. Do not summarise or abbreviate.'
PROMPT_2='Reply with exactly the word OK. Nothing else. Do not use any tool.'

echo "== turn 1 =="
printf '%s' "$PROMPT_1" | codex exec --json \
  -C "$WS" \
  -c model_reasoning_effort="low" \
  -s workspace-write \
  - > "$OUT/turn1.ndjson" 2> "$OUT/turn1.stderr" || {
    echo "turn 1 failed (exit $?); see $OUT/turn1.stderr — STOP, do not retry" >&2; exit 1; }

if grep -qiE 'usage limit|rate limit|quota' "$OUT/turn1.stderr" "$OUT/turn1.ndjson" 2>/dev/null; then
  echo "usage limit hit on turn 1 — STOP and report; do not retry" >&2; exit 2
fi

# resume takes no -s/-o/-C/--add-dir; run from the workspace, sandbox via -c.
echo "== turn 2 (resume) =="
( cd "$WS" && printf '%s' "$PROMPT_2" | codex exec resume --last --json \
    -c model_reasoning_effort="low" \
    -c sandbox_mode="workspace-write" \
    - ) > "$OUT/turn2.ndjson" 2> "$OUT/turn2.stderr" || {
    echo "turn 2 failed (exit $?); see $OUT/turn2.stderr — STOP, do not retry" >&2; exit 1; }

cat "$OUT/turn1.ndjson" "$OUT/turn2.ndjson" > "$OUT/raw.ndjson"

echo "== what the capture settled =="
python3 - "$OUT" <<'PY'
import json, sys, pathlib
out = pathlib.Path(sys.argv[1])
for name in ("turn1.ndjson", "turn2.ndjson"):
    rows = [json.loads(l) for l in (out / name).read_text().splitlines() if l.strip()]
    threads = {r.get("thread_id") for r in rows if r.get("type") == "thread.started"}
    starts = [r for r in rows if r.get("type") == "turn.started"]
    done = [r for r in rows if r.get("type") == "turn.completed"]
    print(f"{name}: {len(rows)} events, turn.started={len(starts)}, turn.completed={len(done)}, thread.started={threads or '(none — resumed)'}")
    for d in done:
        print(f"    usage: {json.dumps(d.get('usage'))}")
print()
def usage(name):
    rows = [json.loads(l) for l in (out / name).read_text().splitlines() if l.strip()]
    return next((r["usage"] for r in rows if r.get("type") == "turn.completed"), None)
u1, u2 = usage("turn1.ndjson"), usage("turn2.ndjson")
if u1 and u2:
    o1, o2 = u1.get("output_tokens", 0), u2.get("output_tokens", 0)
    print(f"turn 1 output_tokens={o1}  turn 2 output_tokens={o2}")
    print("VERDICT:", "CUMULATIVE — normaliser must NOT re-accumulate"
          if o2 >= o1 else "PER-TURN INCREMENT — normaliser MUST accumulate before emitting")
    print("(turn 2 was asked for one word; a large output_tokens can only be turn 1's carried forward.)")
else:
    print("VERDICT: undetermined — a turn.completed is missing; report rather than infer.")
PY

# Draft provenance, so the committed fixture is not hand-assembled under time pressure.
python3 - "$OUT" <<'META'
import json, pathlib, sys, collections
out = pathlib.Path(sys.argv[1])
rows = [json.loads(l) for l in (out / "raw.ndjson").read_text().splitlines() if l.strip()]
kinds = collections.Counter(
    f"{r.get('type')}/{(r.get('item') or {}).get('item_type') or (r.get('item') or {}).get('type')}"
    if r.get("type", "").startswith("item.") else r.get("type", "?")
    for r in rows
)
turns = sum(1 for r in rows if r.get("type") == "turn.completed")
(out / "meta.toml.draft").write_text(f"""# Capture provenance. DRAFT -- complete the scrub, then rename to meta.toml.
# Recorded per onsager-ai/umwelt#20.

harness = "codex"
cli_version = "codex-cli <fill in: codex --version>"
captured_at = "<fill in: YYYY-MM-DD>"
model = "codex default (not pinned; -m was deliberately NOT passed)"
command = "codex exec --json, then codex exec resume --last --json, one thread"
prompt_source = "stdin, both turns"
events = {len(rows)}
turns = {turns}

# The gap this capture was run to close: a single-turn capture cannot tell a
# session-cumulative usage total from a per-turn increment, which is the stated
# precondition of onsager-ai/ethogram#6's accumulation ruling.
[notes]
event_kinds = "{dict(kinds)}"
verdict = "<fill in: CUMULATIVE or PER-TURN INCREMENT, from the output_tokens comparison above>"

[scrub]
# Required before committing, matching the codex/main fixture exactly.
narration = "replaced, exact length preserved"
ids = "real (thread_id, item_N)"
paths = "absolute workspace paths rewritten under /workspace"
""")
print(f"draft provenance written to {out / 'meta.toml.draft'} ({len(rows)} events, {turns} turns)")
META

echo
echo "Raw capture at $OUT/raw.ndjson. NOT yet scrubbed -- scrub before committing:"
echo "  narration replaced by placeholder of the SAME byte length; ids real; absolute paths -> /workspace"
echo "Then rename meta.toml.draft to meta.toml and commit the directory as"
echo "  crates/umwelt-capture/tests/fixtures/codex/two-turns/   (or umwelt/crates/... inside ostrom, post-fold)"
