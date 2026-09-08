# Policy manifest signing

Ostrom trusts a policy manifest for its signature, not its filesystem
location. Every production load requires a detached `<manifest>.sig` and a
separately provisioned directory of trusted RSA public keys.

The signing principal keeps a 2048-bit or larger RSA private key outside every
loop substrate. Sign a manifest after reviewing the root and all of its
includes:

```sh
ostrom sign \
  --key-id policy-principal \
  --key /principal-only/policy-private.pem \
  /policy/ostrom.yaml
```

The command parses and resolves includes before signing, then atomically
writes `/policy/ostrom.yaml.sig`. Cross-scope references are validated when the
signed repository and operator documents are composed. The signature uses RSA PKCS#1 v1.5 with
SHA-256 over a deterministic encoding of the fully composed manifest. YAML
formatting and mapping order are not trusted data; changing any semantic value
in the root or an included leaf invalidates the signature.

Install only the matching public key on a loop substrate. A signature with
`key_id: policy-principal` resolves to `policy-principal.pem` within the
trusted-key directory:

```sh
export OSTROM_POLICY_TRUSTED_KEYS=/run/ostrom/trusted-policy-keys
ostrom validate /policy/ostrom.yaml
```

## Unsigned drafts

To compose or validate a candidate without a signing key at hand, explicitly
pass `--unsigned`:

```sh
ostrom compose --unsigned /policy/ostrom.yaml
ostrom validate --unsigned --strict /policy/ostrom.yaml
```

Unsigned mode writes nothing. It never creates `versions/<digest>`, moves
`current`, or changes any other file under `OSTROM_HOME`. It uses the same
composition and validation checks as the signed commands, skipping only the
candidate's signature verification. The composed manifest and digest are
identical for the same authored files, even if a signature is already present.
A separately loaded operator context still requires a valid signature.

The flag is argv-only: no environment variable, manifest field, configuration
file, or default enables it. Unsetting or emptying `OSTROM_POLICY_TRUSTED_KEYS`
without the flag still refuses to load policy. Other commands and snapshot
loads continue to require signatures.

Compose keeps its existing stdout line, `composed digest=<digest> path=<path>`.
In unsigned mode that path names where the version would be installed; nothing
is written there. A separate stderr line states that composition was performed
without verifying the candidate's signature and that nothing was written.
Validation keeps its resolution-context output and honours `--strict` and
`--normalized` as usual.

Trust still follows the signature, not the filesystem. An unsigned draft does
not become trusted policy; sign it and use signed composition to adopt it.

## Resolution context

`ostrom validate` resolves a manifest's references in a context. With one — the
operator manifest found via `OSTROM_HOME`, or an explicit `--operator <file>` —
a grant or loop may name an actor or operation the operator manifest declares,
and validation resolves it there. Composition already did this, so the two now
reach the same verdict on the same input.

Without a context, a reference that could only resolve elsewhere is not an
error. It is reported as unresolved *here* and validation still exits 0:

```sh
ostrom validate /policy/ostrom.yaml
valid: /policy/ostrom.yaml (isolated; 1 unresolved)
unresolved: grants.delegated.actors -> builder
```

The first line always names the context: `(resolved against operator <path>)`,
`(isolated)`, `(isolated; N unresolved)`, or — when every reference resolves
but the manifest declares actors or operations of its own —
`(isolated; evaluated as repository policy)`. With no operator to adopt it,
such a manifest is judged as repository policy: its own operations, prompts
and loops are not adopted, so a grant or loop naming one of them can still be
refused even though nothing was unresolved. The context line says so only
when that assumption is load-bearing; a manifest with no actors and no
operations gets the plain `(isolated)` line, unchanged.

`--strict` makes unresolved references a refusal, with the invalid-manifest
exit code. It is the definition of acceptance: a consumer reading only exit
codes should use it, and "validate accepts this manifest" means `--strict`
exited 0. Without it, exit 0 means the file is well formed, which is not the
same claim.

A `--strict` or `compose` refusal that only holds under the repository-policy
assumption says so, rather than reporting the symptom alone:

```sh
ostrom validate --strict /policy/ostrom.yaml
as repository policy: grant `delegated` names unknown operation `work`
```

With `--normalized`, stdout is the composed YAML document and nothing else;
the context and `unresolved:` lines go to stderr so stdout stays parseable.

`ostrom compose` now applies the same input resolution, selector findings and
adjacent-policy check that validation applies. These previously ran only in
`ostrom validate`, so a manifest that composed before may now be refused at
composition for a fault it always had. This is a behaviour change for anything
pinning this CLI.

`ostrom validate` reports repository actor declarations as portability
findings, naming each actor's source file, while still exiting successfully.
The operator manifest is the roster-owning layer, so actor declarations there
are not findings. `ostrom explain` renders the same repository findings under
its separate `ACTOR PORTABILITY` heading.

To derive policy for one repository from the adopting operator manifest:

```sh
ostrom generate owner/repository --output /repository/ostrom.yaml
```

Omit `--output` or use `--output -` to write the generated YAML to stdout. The
output is unsigned, declares no actors, and carries no repository restriction
in its projected rules. Signing and placing it at the repository entrypoint are
separate, intentional adoption steps.

`OSTROM_POLICY_TRUSTED_KEYS` must be provisioned by the host or worker and is
not configurable from the manifest. Loading refuses an unset trust directory,
a missing or malformed sidecar, an unknown key ID, a malformed or undersized
public key, and a signature mismatch. There is deliberately no environment
variable or default path for a private signing key.

Rotation is additive: install the new public key under a new key ID, sign with
that ID, deploy the manifest and sidecar, and remove the old public key only
after no deployed signature names it.
