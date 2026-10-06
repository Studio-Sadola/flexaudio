# Releasing flexaudio

Releases are cut by pushing a version tag (e.g. `v0.3.0`), which triggers the
three workflows in `.github/workflows/release-*.yml`.

```bash
git tag v0.3.0
git push origin v0.3.0
```

## 0.3.0 — not yet released

0.3.0 is not published. Tag `v0.3.0` only when the remaining work is done; the
0.2.0 registry-status table below stays as the last published snapshot until
then.

## Registry status — 0.2.0

| Registry | Status | Notes |
|---|---|---|
| **crates.io** | ✅ published | All nine crates. |
| **PyPI** | ✅ published | Wheels (Linux x64/arm64, macOS arm64, Windows x64) + sdist. |
| **npm** | ⏳ **pending** | Blocked by an npm-side bug — see below. Re-run `release-npm.yml` to finish. |

There are **five npm platforms**: Linux x64/arm64, macOS arm64 (Apple Silicon),
and Windows x64/arm64. There are **four wheel platforms**: Linux x64/arm64,
macOS arm64, and Windows x64, plus the Python source distribution. Windows
arm64 wheels are not built. macOS x64 (Intel) is intentionally not prebuilt;
Intel Mac Rust users still build from source via crates.io.

## Version gate and dry runs

All three release workflows validate versions before any artifact upload or
registry publish. Tags must match `^v[0-9]+\.[0-9]+\.[0-9]+$` exactly; for
example, `v0.3.0` is accepted, while `v0.3.0-rc.1` is rejected. The tag's
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
python3 .github/scripts/check-release-versions.py --version 0.3.0
python3 -m unittest discover -s .github/scripts -p test_check_release_versions.py -v
```

For a dry run, open GitHub Actions, select each release workflow, and choose
**Run workflow** on the intended release ref. Enter the explicit `version`
(for example, `0.3.0`, without `v`) and leave `dry_run` checked. It defaults to
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

## npm is not published yet — how to finish it

The npm packages (`@studio-sadola/flexaudio` + the per-platform packages) build
correctly in CI but cannot be published from CI right now. This is an npm
platform issue, not a problem with this repo:

- Publishing a package requires **2FA or a granular access token with "Bypass
  2FA" enabled**. The "Bypass 2FA" token feature is currently broken
  (npm/cli [#8869](https://github.com/npm/cli/issues/8869),
  [#9268](https://github.com/npm/cli/issues/9268) — both open).
- **Trusted publishing (OIDC) cannot bootstrap a brand-new package** — npm has
  no "pending publisher" equivalent yet (npm/cli
  [#8544](https://github.com/npm/cli/issues/8544)), so the first version can't
  be published over OIDC.

**The first npm publish must be performed interactively by a human with 2FA**
for the main package and each platform package. Use the `.node` binaries from
a `release-npm.yml` dry run, prepare the platform packages with their license
files, and publish the platform packages before the main package. CI cannot
bootstrap these new packages through OIDC.

The `NPM_TOKEN` secret and workflow are already in place. Once "Bypass 2FA"
tokens work, re-create `NPM_TOKEN` as a granular token with Bypass 2FA enabled
for subsequent CI releases. After the first successful interactive publish,
OIDC trusted publishing can instead be configured per package at
`npmjs.com/package/<name>/access` for subsequent releases.
