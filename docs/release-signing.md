# Release signing and updates

Flotilla release artifacts are signed with a long-lived Ed25519 key. The
private key exists **only** as the GitHub Actions secret
`FLOTILLA_SIGNING_KEY`; it is never in the repository, never on a workstation
after setup, and never printed by the tooling. The client ships with the
matching public key compiled in.

## The key

| | |
|---|---|
| Algorithm | Ed25519 (detached signature over raw file bytes) |
| Key id | `d901808076693bdc` |
| Public key | `IXfEN939GNZL0F7bRpAqzuHUuoCIKXIfvAB60KKUWw4=` |
| Secret name | `FLOTILLA_SIGNING_KEY` (repository secret, Actions only) |
| Secret format | base64 of the 32-byte Ed25519 seed, one line |
| Client trust list | `crates/client/flotilla/src/updater/signature.rs` → `TRUSTED_KEYS` |

The key id is the first 8 bytes of SHA-256 over the 32 public-key bytes, in
hex. It is written into every manifest (`signing.key_id`) so the client can
select which compiled-in key must verify the document.

## What is signed

Every published release of `The-Coaxys/flotilla` contains:

- `latest.json` — the release manifest (see below);
- `latest.json.sig` — its detached signature;
- `<artifact>.sig` — a detached signature for each artifact, so a manually
  downloaded `Flotilla-Setup-x64.exe` can be checked without trusting
  `latest.json`;
- `SHA256SUMS.txt` and `SHA256SUMS.txt.sig`.

A `.sig` file is the base64 encoding of the 64-byte Ed25519 signature over the
exact bytes of the file it sits next to, with a trailing newline. `fl-sign`
verifies strictly (`verify_strict`), so a signature that does not match the
bytes is a hard failure.

### `latest.json`

```json
{
  "schema": 1,
  "product": "flotilla",
  "version": "0.2.0",
  "tag": "v0.4.0",
  "published_at": "2026-11-04T16:35:39Z",
  "signing": { "algorithm": "ed25519", "key_id": "d901808076693bdc" },
  "assets": [
    { "platform": "windows-x64", "kind": "client", "name": "flotilla-x64.exe",
      "sha256": "<64 hex>", "size": 12345678 },
    { "platform": "windows-x64", "kind": "installer", "name": "Flotilla-Setup-x64.exe", ... },
    { "platform": "windows-x64", "kind": "licenses", "name": "flotilla-windows-licenses.zip", ... },
    { "platform": "linux-x86_64", "kind": "daemon", "name": "flotillad-x86_64-unknown-linux-musl", ... },
    { "platform": "linux-aarch64", "kind": "daemon", "name": "flotillad-aarch64-unknown-linux-musl", ... }
  ]
}
```

- `version` is the product version the binary itself reports
  (`PRODUCT_VERSION`, from the workspace `Cargo.toml`). It is the ordering key
  and the value the post-restart self-check compares against.
- `tag` is the git tag the artifacts were published under. Milestones are
  tagged independently of the crate version (`v0.4.0` may ship a binary that
  reports `0.2.0`), so `tag` is deliberately **not** tied to `version`; it only
  names where the assets live.
- `assets[].kind == "client"` is the artifact the updater installs on its
  `platform`. The installer and the daemons are listed so other update paths
  (and humans) can verify them from the same signed document.

## Generating and rotating the key

`scripts/fl-sign` is a standalone crate (it is not part of the product
workspace; CI builds it with `--manifest-path`).

```bash
cargo build --release --manifest-path scripts/fl-sign/Cargo.toml
sign=./scripts/fl-sign/target/release/fl-sign

# 1. New key pair. The private key is written once, mode 0600, outside the repo.
$sign generate --key-out ~/.flotilla-signing/flotilla-signing.key
#    -> prints public_key=<base64> and key_id=<hex>

# 2. Store it as the Actions secret, straight from the file (never on a command line).
gh secret set FLOTILLA_SIGNING_KEY -R The-Coaxys/flotilla < ~/.flotilla-signing/flotilla-signing.key
shred -u ~/.flotilla-signing/flotilla-signing.key    # after you are sure the secret works
```

Rotation is two releases, by design:

1. **Trust the new key:** add the new `(key_id, public_key)` pair to
   `TRUSTED_KEYS` in `crates/client/flotilla/src/updater/signature.rs` and ship
   a client that trusts both keys.
2. **Switch signing:** replace `FLOTILLA_SIGNING_KEY` with the new seed. CI
   refuses to publish when the secret does not match a key embedded in the
   client, so a clobbered secret fails loudly instead of publishing something
   clients reject.
3. **Retire the old key:** after every supported client has been updated,
   remove the old entry from `TRUSTED_KEYS`.

A lost key cannot be recovered — that is the point of keeping it only in the
secret store. Rotate as above; clients that only trust the lost key will refuse
new manifests until they are updated by hand.

## What CI does

`.github/workflows/ci.yml`:

- **`release`** (push to a `v*` tag, or `main` for the `nightly` prerelease):
  collects the built artifacts, writes `SHA256SUMS.txt`, verifies the assets
  against it, refuses to run without the secret, checks that the secret matches
  the key embedded in the client, writes and signs `latest.json`, signs every
  artifact, verifies all signatures with the public key, publishes, then
  **downloads the published release again** and re-runs `sha256sum -c` and
  `fl-sign verify` against it.
- **`signing`** (`workflow_dispatch` only): the same assemble/sign/verify
  steps with `--tag dry-run`, uploaded as the `signed-release-dry-run`
  artifact and never published. This is how the signing path is exercised
  without a release.

Verify a release by hand:

```bash
gh release download v0.4.0 -R The-Coaxys/flotilla -D verify
(cd verify && sha256sum -c SHA256SUMS.txt)
$sign verify IXfEN939GNZL0F7bRpAqzuHUuoCIKXIfvAB60KKUWw4= verify/latest.json verify/flotilla-x64.exe
```

## Client update flow

`crates/client/flotilla/src/updater/` runs on its own worker thread and never
touches the network from the UI thread.

1. **Check.** `GET {release-base}/latest/download/latest.json` and
   `…/latest.json.sig` (release base:
   `https://github.com/The-Coaxys/flotilla/releases`). A missing release (404)
   is reported as "no release yet", not as an error.
2. **Verify before parsing.** The `key_id` inside the document only selects
   which compiled-in key must verify the raw bytes; unknown keys, malformed
   signatures and any byte-level difference are refused, and nothing is
   downloaded for a refused manifest.
3. **Validate.** Schema, product, plain tag/asset names, per-asset sha256,
   size bounds (≤ 512 MiB), exactly one client asset for the platform, and the
   version must be strictly newer than the running one — a downgrade or the
   same version is refused.
4. **Download.** To `<data dir>/Flotilla/updates/<tag>/<name>.part`, resumable
   with HTTP `Range` (a server that ignores the range restarts the file),
   bounded by the manifest size, and only renamed to its final name after the
   size and SHA-256 match. A mismatch deletes the partial file and refuses.
5. **Stage.** The verified asset stays in the staging directory; the installed
   binary is not touched.
6. **Replace, on explicit user action only.** The candidate is copied next to
   the installed binary, the installed binary is renamed to
   `flotilla.exe.previous`, the candidate is renamed into place, and the new
   build runs `--self-test=<tmp>`; if that report is missing or fails, the
   previous build is renamed back immediately and the failed candidate is kept
   as `flotilla.exe.failed`. `update-pending.json` records the swap.
7. **Confirm after the first window frame.** Startup checks `update-pending.json`
   and the new binary's self-test, but keeps the backup until GPUI renders the
   main window. The marker records an attempted launch. If that launch exits
   before rendering, the next start restores and launches the previous build,
   even when the new binary's headless self-test passes. A mismatched version
   cannot confirm another build's replacement. The two rename operations are
   not a filesystem transaction: termination between them can require launching
   `flotilla.exe.previous` manually.
8. **Prompt.** A non-modal banner shows available / downloading / staged /
   failed, with a `Restart now` button only when no turn, in-flight mutation,
   syncing terminal or unsaved editor is present. Unknown/stale chat activity
   also blocks restart. A restart always requires a click, with a `Later` dismissal;
   there is no silent auto-restart. A failure says the current build keeps running.

Environment overrides:

| Variable | Effect |
|---|---|
| `FLOTILLA_NO_UPDATE_CHECK=1` | Do not start the update worker (selftests, CI, unsupported installs). |
| `FLOTILLA_UPDATE_FEED=<base>` | Use another release base (a mirror, or a local server for manual testing). |

Logs go to `%LOCALAPPDATA%\Flotilla\logs\flotilla.log`; update decisions,
refusals and rollbacks are recorded there with their reason.
