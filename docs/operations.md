# Policy operations

Operations are the only policy-authored capability an agent can invoke. The
active manifest is `${OSTROM_POLICY_MANIFEST}` when set and otherwise
`<Ostrom config>/ostrom.yaml` (with `ostrom.yml` accepted on read). The harness
sets `OSTROM_ACTOR`; the positional target is resolved before the scope-blind
grant/deny decision. Operations and loops execute only from this adopting
operator manifest; same-named repository declarations are inert. Every load is
signature-gated as described in [Policy manifest signing](policy-signing.md).

```yaml
manifest_version: 1
actors:
  gatekeeper: {}
operations:
  merge:
    name: Merge
    params:
      note: {type: markdown}
    steps:
      - uses: gh/post-verdict
        with: {note: $params.note}
      - uses: gh/merge-pr
        with: {method: squash}
        requires: ready-to-merge
grants:
  gatekeeper-merge:
    actors: gatekeeper
    operations: merge
```

Invoke it as one indivisible sequence:

```sh
ostrom merge placeholder-org/repository#7 --note @verdict.md
```

`markdown` values may flow only to content sinks. `semver` and a declared
`enum` may flow to command arguments. References are whole YAML values in the
form `$params.name`; interpolation is not supported. A `cmd/run` script must be
literal policy content and can never reference a caller parameter.

The closed operation action catalogue is:

| action | boundary | scope | guard |
|---|---|---|---|
| `agent/claude` | local | none | optional |
| `gh/post-verdict` | mediated | `pull_requests:write` | optional |
| `gh/merge-pr` | mediated | `contents:write,pull_requests:write` | required |
| `git/tag` | mediated | `contents:write` | required |
| `cmd/run` | local | none | optional |
| `sys/enable-loop` | ungrantable | none | unavailable |

A mediated action mints its own scoped installation token and exposes it only
to that action's child process. A local action removes `GH_TOKEN` and
`GITHUB_TOKEN` from its child environment. The ungrantable catalogue entry has
no dispatcher and makes any operation containing it invalid.

The operation-side `agent/claude` input is limited to a required non-empty
`prompt` and an optional non-empty `model`. Its permission profile and
permission mode come from the invoking actor; neither is a step parameter.

`ostrom operations` lists declarations. `ostrom operations --actor builder`
lists the operations with at least one builder grant. The settings preview and
drift check are non-installing commands:

```sh
ostrom operations --settings builder
ostrom operations --actor builder --check-settings /path/to/builder.settings.json
```

Generated settings use `defaultMode: deny`, set `OSTROM_ACTOR`, contain one
`Bash(ostrom <operation> *)` allow per grant, and contain no deny list. These
commands print a profile; `ostrom pass` derives and installs its own from the
same grants when the manifest declares its actor.
