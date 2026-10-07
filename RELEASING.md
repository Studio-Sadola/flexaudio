# Releasing flexaudio

Releases are cut by pushing a version tag (e.g. `v0.3.1`), which triggers the
three workflows in `.github/workflows/release-*.yml`.

```bash
git tag v0.3.1
git push origin v0.3.1
```

## Registry status — 0.3.0

Verified on 2026-10-07: version 0.3.0 is published on all three registries.
The next patch release is 0.3.1.

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

For a dry run, open GitHub Actions, select each release workflow, and choose
**Run workflow** on the intended release ref. Enter the explicit `version`
(for example, `0.3.1`, without `v`) and leave `dry_run` checked. It defaults to
**true** in all three workflows. The selected ref's manifest versions must
match the input. npm validates packaging for the main and all platform
packages; PyPI builds and collects wheels and sdist; crates.io performs the
existing leaf-crate dry runs (dependent crates are validated at real publish).
None of these dry runs publishes a registry package. npm/PyPI build artifacts
are still uploaded to GitHub Actions for inspection.

Uncheck `dry_run` explicitly to publish a manual release. Pushing a valid
release tag triggers real publishing. An npm platform package is skipped only
when `npm view name@version version` succeeds for that exact package version;
any other platform publish failure stops the job before the main package.

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
