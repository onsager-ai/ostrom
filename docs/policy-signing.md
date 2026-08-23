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
