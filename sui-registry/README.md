# sui-registry — porto

An OCI Distribution Spec v1.1 registry server (binary: `porto`). It has two
storage modes, which can run side by side in one process:

- **Writable.** Blobs and manifests are stored in a sui-castore
  `StorageBackend` (local disk, S3, Redis, Postgres, or tiered), the same
  content-addressed store that holds Nix NARs. Tags and referrers are kept in
  memory, so they are lost on restart. The durable fix is a sui-store table,
  and it is still open.
- **Read-only OCI image layouts.** porto serves directories that follow the
  OCI image-layout spec (`oci-layout`, `index.json`, `blobs/<alg>/<hex>`),
  normally immutable `/nix/store` paths built by Nix. This is how every fleet
  node can serve internal Helm charts at
  `oci://charts.pleme.internal/pleme-io/charts/<chart>:<version>`.

## Read-only layout mode

```yaml
# porto.yaml — PORTO_TIER=/etc/porto/porto.yaml porto
listen: 0.0.0.0:5000
layouts:
  - repository: pleme-io/charts
    layout: /nix/store/<hash>-pleme-io-charts   # an OCI image layout
    verify: eager                               # eager (default) | lazy
  - repository: pleme-io/charts                 # several layouts may feed one repository
    layout: /nix/store/<hash>-more-charts
# backend: { type: local, path: /var/cache/porto }   # optional: writable repos too
```

**Tags.** Tags come from the `org.opencontainers.image.ref.name` annotation on
each `index.json` entry, so they survive a restart without being stored
anywhere:

| `ref.name`           | served as                                    |
|----------------------|----------------------------------------------|
| `0.1.0`              | `<repository>:0.1.0`                         |
| `pleme-porto:0.1.0`  | `<repository>/pleme-porto:0.1.0`             |
| absent               | in `<repository>`, by digest only            |

With the second form, one layout can hold a whole chart repository. A `ref.name`
that is not `<tag>` or `<path>:<tag>`, with a valid OCI tag and repository name,
is a startup error. Note that `+` is not legal in an OCI tag; Helm writes semver
build metadata with `_` instead.

**What is served.** porto serves the catalog (`GET /v2/_catalog`), tag lists,
manifests by tag or digest (GET and HEAD, using the descriptor's `mediaType` and
`Docker-Content-Digest`), blobs, and referrers (taken from manifests that have a
`subject`). Nested image indexes are followed. A blob is served only if a loaded
manifest references it.

**Writes are refused.** porto checks for a read-only repository before it runs
any write handler. Every `POST`, `PUT`, `PATCH` or `DELETE` on such a repository
returns `403` with the OCI code `DENIED`. If a writable `backend` is also
configured, other repository names accept pushes, but a repository served from a
layout stays read-only, so a push can never replace a chart built by Nix.

**Conflicts.** If two layouts (or two entries in one `index.json`) give the same
`repository:tag` different digests, porto refuses to start
(`LayoutError::ConflictingTag`). It never picks one by load order. If they give
the same digest, the entries simply agree.

**Corruption.** porto hashes every manifest at load. Each layout's `verify`
setting controls when blobs are checked against their digests:

- `eager` hashes them at load, streaming, so a corrupt store path stops startup.
- `lazy` hashes a blob on its first read and caches the result. A blob that does
  not match is refused with a logged 500, and no byte of it is sent.

In both modes, porto checks each file's size against its descriptor at load.

**No writable backend.** If `layouts` is set and `backend` is not, porto is a
pure read-only mirror. It does not create the default `/var/cache/porto` store.

## Configuration

`RegistryConfig` implements `shikumi::TieredConfig`. `PORTO_TIER` selects the
tier:

- unset or `default`: the prescribed tier (`0.0.0.0:5000`)
- `bare`: loopback, ephemeral port, nothing mounted
- a path: a YAML overlay on top of the prescribed tier

porto parses the overlay file strictly. If the file is missing, malformed, or
has an unknown key, porto exits with a non-zero status. It does not fall back to
defaults, because a fallback would start a server with no layouts mounted.
After the tier is chosen, `PORTO_LISTEN`, `PORTO_MAX_BODY_BYTES`, `PORTO_BACKEND`
(JSON) and `PORTO_STORE_PATH` override individual fields.

## Commands

| command               | does                                                              |
|-----------------------|-------------------------------------------------------------------|
| `porto` / `porto serve` | serve                                                           |
| `porto check`         | load and verify every layout, list repositories, exit (CI / deploy gate) |
| `porto config-schema` | print the config's JSON Schema (input to substrate `types.jsonSchema`) |
| `porto config-show`   | print the resolved config as YAML                                 |

## Nix

Build or run it with `nix build github:pleme-io/sui#porto` or
`nix run github:pleme-io/sui#porto`.

## Tests

`tests/oci_conformance.rs` runs the writable spec matrix over both stores.
`tests/layout_mode.rs` runs layout mode against a fixture that contains a real
Helm chart. Its `helm_pull_round_trips_the_chart` test runs the real `helm pull
--plain-http` against a live listener. That test is `#[ignore]`d because CI does
not always have `helm`, so run it with:

```text
cargo test -p sui-registry --test layout_mode -- --include-ignored
```
