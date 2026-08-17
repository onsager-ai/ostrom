# Implementer binary comparison window

`ostrom dispatch` deliberately launches `plugins/ostrom/scripts/implement.sh`
by default during the production comparison window. Select the Rust
implementation explicitly with:

```sh
MANDATE_IMPLEMENTER_ENGINE=rust ostrom dispatch /path/to/work-order.json
```

The transient unit then runs `ostrom implement <work-order-file> <unit-name>`.
`MANDATE_OSTROM_BIN` may name an absolute CLI path if `ostrom` is not on the
unit's `PATH`.

To roll back, unset `MANDATE_IMPLEMENTER_ENGINE` or set it to `shell`. The
existing `MANDATE_IMPLEMENTER_BIN` override selects an alternate shell
implementer only when the shell engine is active.

After the comparison window, cutover is the one-line change from `"shell"` to
`"rust"` in `DEFAULT_IMPLEMENTER_ENGINE`; deleting the fallback script remains
a separate retirement step.
