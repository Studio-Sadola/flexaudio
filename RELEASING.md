# Releasing flexaudio

Releases are cut by pushing a version tag (e.g. `v0.2.0`), which triggers the
three workflows in `.github/workflows/release-*.yml`.

```bash
git tag v0.2.0
git push origin v0.2.0
```

## Registry status — 0.2.0

| Registry | Status | Notes |
|---|---|---|
| **crates.io** | ✅ published | All nine crates. |
| **PyPI** | ✅ published | Wheels (Linux x64/arm64, macOS arm64, Windows x64) + sdist. |
| **npm** | ⏳ **pending** | Blocked by an npm-side bug — see below. Re-run `release-npm.yml` to finish. |

Prebuilt binaries cover Linux x64/arm64, macOS arm64 (Apple Silicon), and
Windows x64. macOS x64 (Intel) is intentionally not prebuilt — Intel Mac Rust
users still build from source via crates.io.

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

**To finish once npm ships a fix:** the `NPM_TOKEN` secret and the workflow are
already in place. When "Bypass 2FA" tokens work, re-create `NPM_TOKEN` as a
granular token with Bypass 2FA enabled and re-run `release-npm.yml`. After the
first successful publish, switch to OIDC trusted publishing (configure it per
package at `npmjs.com/package/<name>/access`) for subsequent releases.

Alternatively, the first version can be published interactively from a machine
with 2FA, using the `.node` binaries produced by the `release-npm.yml` build job.
