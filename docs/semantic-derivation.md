# Semantic derivation in the mandate sweep

The mandate sweep has two deliberately unequal stages:

1. Mechanical selectors produce `classification` and `matched_selector` and
   remain the only authorization decision.
2. An optional model reads a changed item's title, labels, body, and comments
   and emits advisory `semantic_derivation` findings beside that decision.

The advisory contract contains positive findings named `parked`,
`already_decided`, `genuinely_stuck`, and `actually_a_release`. Every finding,
and every optional authority escalation, carries a confidence from 0 through 1
and a non-empty quote copied verbatim from its named title, label, body, or
comment source. Output is rejected if its structure is different or a quote is
not present in that source.

The harness, rather than the prompt, enforces the authorization boundary. A
model may advise `unclassified`, `tripwire`, or `reserved`; it may never advise
`delegated` or `excluded`, and it may not replace an existing mechanical
`tripwire` (including a bounce match) or `reserved` result. The mechanical
fields are never mutated. Public issue text and comments are therefore data,
not instructions that can widen unattended authority.

The stage is absent unless either `MANDATE_SEMANTIC_DERIVER` names an executable
port or `ANTHROPIC_API_KEY` is present. The bundled adapter defaults to
`claude-haiku-4-5-20251001`: Haiku is the small, fast tier suited to bounded
fact extraction, while larger models add cost without changing the mechanical
safety boundary. `MANDATE_SEMANTIC_MODEL` swaps the model without editing call
sites, and no credential is stored in configuration or state.

Verdicts, including rejected verdicts, are cached by a SHA-256 hash of exactly
the title, labels, body, and fetched comments. The hash and model result do not
enter the existing item fingerprint. Comments are fetched only for records the
incremental cursor reports as new or changed; unchanged text is not re-judged
and cannot manufacture a queue movement event.
