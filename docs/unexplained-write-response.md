# Responding to an unexplained write

An `unexplained-write` queue row is an alarm: Ostrom observed a merge or a
pushed `ostrom/` branch that its local execution records do not account for.
Detection does not block or undo the write.

1. Rotate the affected GitHub App private key, then retire the old key through
   the normal organisation credential procedure.
2. Review the GitHub organisation audit log around the write time, especially
   installation-token requests and the actions performed with those tokens.
3. Preserve and inspect the local records in this order:
   `ostrom/sprint.jsonl` (the trace), `ostrom/work-orders/` (durable work
   orders), and `ostrom/queue.jsonl` (the rendered alarm and its scope
   evidence).

Compare the repository, item, order, branch, head SHA, and timestamps across
those records. Do not place App keys, tokens, or other credential values in an
issue, queue annotation, or incident note.
