# Releasing flexaudio

Releases are cut by pushing a version tag (`v<version>`), which triggers the
three workflows in `.github/workflows/release-*.yml`.

```bash
git tag v<version>
git push origin v<version>
```

## Before tagging

- Run `python3 .github/scripts/check-release-versions.py --workspace`; the
  manifest and documentation version checks must pass for the intended tag.
- Have two independent reviewers audit every documentation file against the
  code: versions, names, behavior, missing features on each registry page, and
  Japanese/English agreement. Fix every finding before continuing.
- Complete the release checks below, then create and push the version tag.

There are **five npm platforms**: Linux x64/arm64, macOS arm64 (Apple Silicon),
and Windows x64/arm64. There are **four wheel platforms**: Linux x64/arm64,
macOS arm64, and Windows x64, plus the Python source distribution. Windows
arm64 wheels are not built. macOS x64 (Intel) is intentionally not prebuilt;
Intel Mac Rust users still build from source via crates.io.

## Version gate and dry runs

CI checks published Rust APIs against their latest crates.io releases on native OS runners.
Node/Python declarations and the generated C header are checked against the latest release tag.
Equal versions and patch bumps reject removed or changed declarations; pure additions are allowed.
These declaration checks do not prove runtime behavior or binary ABI compatibility.
Breaking changes require a minor bump for 0.x releases and prior maintainer approval.

TypeScript and C declarations are compared as whole signatures. Python functions may
extend an older signature on a patch release when every existing caller stays valid:

- Decorators, the return annotation, and existing `*args` / `**kwargs` stay the same.
- Existing positional parameters keep their names, order, annotations, defaults, and
  whether each one is positional-only or positional-or-keyword.
- Existing keyword-only parameters keep their names, annotations, defaults, and
  relative order.
- New positional parameters are allowed only after the existing ones, and each new
  one must have a default.
- New keyword-only parameters must have defaults. They may be added before, between,
  or after the existing keyword-only parameters.
- Adding `*args` or `**kwargs` where the old signature had none is allowed.

Any other edit is rejected. That includes removing, renaming, or reordering a
parameter; changing an annotation or a default; adding a required parameter;
removing `*args` or `**kwargs`; changing the return annotation; and changing a
decorator.

A reviewed exception is listed in `.github/api-compatibility-exceptions.json`.
Each entry has exactly four fields: `file`, `name` (the qualified name the gate
prints, such as `StreamVadOptions.tap`), `baseline` (a tag such as `v0.6.2`), and
`reason`. An entry suppresses one reported difference only when the gate's baseline
tag is exactly that `baseline`, so the entry stops applying once a newer release
tag is the baseline. The gate rejects unknown fields, missing or empty fields, a
baseline that is not `vMAJOR.MINOR.PATCH`, duplicate entries, and an entry for the
current baseline that does not match a reported difference.

All three release workflows validate versions before any artifact upload or
registry publish. Tags must match `^v[0-9]+\.[0-9]+\.[0-9]+$` exactly; for
example, `v0.3.1` is accepted, while `v0.3.1-rc.1` is rejected. The tag's
version must equal the workspace version in `Cargo.toml`, the Python project
version in `bindings/flexaudio-py/pyproject.toml`, the main npm package version,
every npm platform package version, and all main npm `optionalDependencies`
versions. Missing or malformed version fields fail the gate.

The same gate checks tracked READMEs, Markdown under `docs/`, and Node, Python,
and C declaration comments. Installation requirements and "Pin to" advice must
use the release's minor or exact version; pip ranges may use the next minor as
an exclusive upper bound. Registry README repository links must use the exact
`v<version>` tag, and relative links must stay inside the package directory.
CI uses `--workspace` to read the version from `Cargo.toml`; release workflows
use the tag version, including for resolved-tag publication retries.

Only `CHANGELOG.md`, sections under an exact `History` or `Migration` heading,
and lines containing the whole word `history`, `since`, `previously`, or the
phrase `migration from` (case insensitive) exempt historical version references.
An exemption ends at the next heading of the same or higher level. Historical
sections do not exempt broken relative registry links.

Platform manifests under `crates/flexaudio-napi/npm/` are generated during the
npm release; a source checkout may have none. After generation the workflow
runs the same gate again with `--require-platforms`, which also requires the
complete platform package set to match `optionalDependencies`. Each platform
package includes `LICENSE` and `THIRD_PARTY_NOTICES.md`; the workflow copies
these after generation, adds them to `files`, and verifies their presence
before publishing or running packaging validation.

Check a checkout locally with Python 3.11 or newer:

```bash
python3 .github/scripts/check-release-versions.py --workspace
python3 .github/scripts/check-release-versions.py --version <version>
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

## Deprecation policy

A public name, field, option or function that is replaced is first marked
deprecated: Rust `#[deprecated]`, TypeScript `@deprecated`, Python
`DeprecationWarning`, or a C header comment. It keeps working for at least one
minor release before removal. Removal is a breaking change and follows the
version rules above, including prior maintainer approval. The CHANGELOG lists
deprecations under `Deprecated`.

## Registry completion and ownership

A release is complete only when **Verify registry completion** is green.
This final job in `release-npm.yml` runs after npm publication and waits for the
tag-push crates.io and PyPI workflows for the same tag and commit to complete.
Keeping it in the existing npm workflow avoids a separate privileged
`workflow_run` trigger. The job has only `contents: read` and `actions: read`,
uses no secrets, and runs even when publication fails so missing artifacts
remain visible. Manual publication retries do not run this job; after retrying,
run the checker below to confirm completion.

The Python 3.11+ standard-library checker derives publishable crates from Cargo
manifests (excluding `publish = false`), npm packages from the main manifest and
its `optionalDependencies`, and the Python package from `pyproject.toml`.
It queries HTTPS registry JSON directly, including registry.npmjs.org regardless
of npm client configuration. Every crate and npm package must expose the version;
PyPI must expose at least one wheel and one sdist. Yanked distributions do not
count. The checker prints each artifact as found, missing or error, retries for
up to 900 seconds by default, and fails on unresolved absence or unreadable
responses. Each request has a bounded timeout; the final inspection can add
at most one short request per remaining artifact beyond the propagation wait.

```bash
python3 .github/scripts/check_registry_completion.py --version <version>
python3 .github/scripts/check_registry_completion.py --version <version> --wait-seconds 0
```

Use `--wait-seconds` and `--interval-seconds` to adjust propagation polling.
The workflow waits up to 90 minutes for the other publish workflows, then
allows the default 15-minute registry propagation window.

`.github/CODEOWNERS` records `@tsubome` as owner of all files and requests review.
Enforcement requires branch protection, which is not enabled.

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
