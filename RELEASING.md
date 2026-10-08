# Releasing flexaudio

Releases are cut by pushing a version tag (e.g. `v0.3.1`), which triggers the
three workflows in `.github/workflows/release-*.yml`.

```bash
git tag v0.3.1
git push origin v0.3.1
```

## Registry status — 0.3.0

Verified on 2026-10-07: version 0.3.0 is published on all three registries.
The next release is 0.4.0, which includes breaking API changes.

| Registry | Status | Notes |
|---|---|---|
| **crates.io** | ✅ published | All nine crates. |
| **PyPI** | ✅ published | Wheels (Linux x64/arm64, macOS arm64, Windows x64) + sdist. |
| **npm** | ✅ published | Main package + all five platform packages, all with npm provenance. |

There are **five npm platforms**: Linux x64/arm64, macOS arm64 (Apple Silicon),
and Windows x64/arm64. There are **four wheel platforms**: Linux x64/arm64,
macOS arm64, and Windows x64, plus the Python source distribution. Windows
arm64 wheels are not built. macOS x64 (Intel) is intentionally not prebuilt;
Intel Mac Rust users still build from source via crates.io.

## Version gate and dry runs

All three release workflows validate versions before any artifact upload or
registry publish. Tags must match `^v[0-9]+\.[0-9]+\.[0-9]+$` exactly; for
example, `v0.3.1` is accepted, while `v0.3.1-rc.1` is rejected. The tag's
version must equal the workspace version in `Cargo.toml`, the Python project
version in `bindings/flexaudio-py/pyproject.toml`, the main npm package version,
every npm platform package version, and all main npm `optionalDependencies`
versions. Missing or malformed version fields fail the gate.

Platform manifests under `crates/flexaudio-napi/npm/` are generated during the
npm release; a source checkout may have none. After generation the workflow
runs the same gate again with `--require-platforms`, which also requires the
complete platform package set to match `optionalDependencies`. Each platform
package includes `LICENSE` and `THIRD_PARTY_NOTICES.md`; the workflow copies
these after generation, adds them to `files`, and verifies their presence
before publishing or running packaging validation.

Check a checkout locally with Python 3.11 or newer:

```bash
python3 .github/scripts/check-release-versions.py --version 0.3.1
python3 -m unittest discover -s .github/scripts -p test_check_release_versions.py -v
```

For npm, **Run workflow** is a publication retry, including when `dry_run` is
checked. Enter the version (for example, `0.3.1`, without `v`) and select a ref
containing the updated workflow/helpers. The workflow resolves that version's
tag to its commit, validates the manifests in that checkout, and selects the
original tag-push `release-npm.yml` run for that tag and commit. It does not
require the original run's overall conclusion to be successful: a publish
failure does not invalidate completed build artifacts.

On tag push, the five build jobs upload the existing `bindings-*` artifacts.
A collection job produces one `npm-release-manifest` artifact containing
`SHA256SUMS`, with exactly the five expected `.node` filenames. Publication
checks the downloaded bytes against this manifest before packaging, then
checks each platform package's embedded `.node` after `napi artifacts` and
before publishing or dry-run packing. Missing/extra files, duplicate names,
unsafe manifest paths, symlinks and hash mismatches fail the job.

**Retry a failed npm release with workflow_dispatch.**
Dispatch skips builds and reseals the original tag-push run's five `bindings-*`
artifacts into a new `npm-release-manifest` in the current run, even if the
original seal failed. Only integer `run_attempt=1` is accepted for the original
run; if it was re-run, resealing is forbidden and a new patch version is required.
Publication verifies those original bytes before and
after packaging. Leave `dry_run` checked to validate packaging; uncheck it to
publish. Already published versions are skipped; expired, deleted or incomplete
addons fail without rebuilding. Do not use **Re-run all jobs** for a retry,
because it can execute the tag-push builds again.

PyPI and crates.io manual dry runs still use the selected ref and require its
manifests to match the input version. PyPI builds and collects wheels/sdist;
crates.io performs leaf-crate dry runs. Leave `dry_run` checked to avoid
registry publication. Pushing a valid release tag triggers real publishing.

PyPI artifact reuse is a follow-up: its Linux container builds, wheel repair,
and sdist need their own distribution checksum contract before dispatch can
safely become a publish-only operation. For a PyPI publish-only failure, use
**Re-run failed jobs** to retain the original successful wheel/sdist jobs.
Its current dispatch still rebuilds wheels/sdist.

## Windows release reproducibility

All release workflows pin Rust to CI's `1.98.1` and disable Cargo incremental
compilation. Cargo/N-API and maturin release builds use `--locked`. MSVC builds
use `/Brepro` and `/INCREMENTAL:NO`; distributed release binaries additionally
use `/DEBUG:NONE` and remap workspace, Cargo home and target paths. Debug
builds retain their debugger symbols. Only Windows N-API release builds opt
into `+crt-static`, preserving their existing CRT policy; Python retains its
dynamic CRT policy.

The CI job **Reproducible Windows x64 N-API release** runs on pull requests and
CI `workflow_dispatch`. It builds the addon twice using separate, fresh target
directories, caches only Cargo registry downloads, prints both SHA-256 hashes,
and fails if they differ. This verifies the current runner/toolchain; hosted
MSVC/SDK image changes and Windows arm64 reproducibility still need observation.

The N-API CLI is pinned to an exact version (`@napi-rs/cli` in
`crates/flexaudio-napi/package.json`). It has no runtime dependencies, so the
workflows install it with `npm install --no-package-lock --omit=optional
--ignore-scripts` from registry.npmjs.org instead of using a lockfile. A
lockfile is deliberately not committed: it would also pin the per-platform
`@studio-sadola/flexaudio-*` optional dependencies, which do not exist on the
registry for a new version until that version is published. To update the CLI,
change the exact version and check the Windows reproducibility job.

Do not push a release tag until the Windows reproducibility check passes.

## Publishing authentication

- **crates.io:** `release-crates.yml` currently uses the repository secret
  `CARGO_REGISTRY_TOKEN`, scoped to the publish job. The workflow's OIDC
  alternative is commented out and is not active.
- **PyPI:** `release-pypi.yml` uses Trusted Publishing via GitHub OIDC and
  `pypa/gh-action-pypi-publish@release/v1`. The publish job has `id-token: write`
  and no active environment binding or API token. Its token-based alternative
  is commented out and is not active.
- **npm:** `release-npm.yml` uses Trusted Publishing via GitHub OIDC for all
  six packages, with `id-token: write` on the publish job and `--provenance`
  on each publish command. No long-lived npm token is needed. The workflow
  upgrades to `npm@latest` and fails if the installed CLI is below 11.5.1.

Each npm package's Trusted Publisher is configured on npmjs.com with:

- Publisher: GitHub Actions.
- Repository: `Studio-Sadola/flexaudio`.
- Workflow file: `release-npm.yml`.
- Environment: none.
- Allowed action: `npm publish` only.

If the workflow file is renamed, re-create the Trusted Publisher configuration
on npmjs.com for the main package and all five platform packages before the
next publish. The workflow filename is part of the publisher identity.

The earlier 0.3.0 npm publishing failures were caused by an expired token.
They were not blocked by the npm CLI bugs previously described here; all six
0.3.0 packages are now published with provenance.
