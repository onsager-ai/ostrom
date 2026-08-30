# Ostrom CLI seam contract

These criteria describe the black-box contract of the shipped `ostrom`
binary. The derivative checks in `duhem.yml` drive that real binary through
Duhem's page-free `cli/invoke` action; every verdict is a deterministic
assertion over process exit codes and output.

## AC-1 — the shipped default composes

The operator policy written by `ostrom init` survives the sequence an
operator actually performs: `sign`, `validate`, then `compose`, without an
edit between those commands.

## AC-2 — validation and composition agree

The shipped, signed default policy accepted by `ostrom validate` is also
accepted by `ostrom compose`. Neither command may present the default as
usable while the other refuses it.

## AC-3 — signatures cover policy content

Changing either the signed manifest's policy content or a prompt file it
references invalidates the signature.

## AC-4 — untrusted policy is refused

An unsigned manifest and a manifest signed by a key outside the configured
trust directory are refused. A signed manifest is also refused when the trust
directory is unset; the CLI must not fall back to ambient trust.

## AC-5 — the binary version matches the release tag

At an exact release tag, `ostrom --version` reports that tag's version.

## Deferred cross-repository criterion — `ostrom pass <name>`

The command's argv and its pass-id, wake-counter, and `pass-ended` state files
are a frozen contract consumed by ostrom-hub. A holistic check needs a real
agent runner and its controlled credentials. Substituting a fixture executable
would mock the very process boundary this Verification Definition is required
to exercise, so this page-free suite does not claim a verdict on that
criterion. It should be added in the deployed-hub VD where the real runner is
available and secrets can remain outside evidence.
