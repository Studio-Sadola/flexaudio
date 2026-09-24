# English summaries of Japanese commit messages

The repository is English-only, but 13 commits made before that rule was applied have
Japanese commit messages. History is not rewritten (rewriting would change every later
commit ID and break forks and open pull requests), so this page gives a short English
summary of each one instead. Commits not listed here already have English messages.

| Commit | Summary |
|--------|---------|
| `924f009` | **ci(release-npm): do not run the prepublishOnly hook on publish.** `napi prepublish -t npm` fired during `npm publish` and failed trying to create a GitHub release and publish the platform packages twice. Publish now passes `--ignore-scripts`, and `prepublishOnly` is removed from `package.json`. |
| `1840fbb` | **ci(release-npm): update the npm CLI before publishing** (needed for staged publishing). |
| `8985e37` | **ci(release-crates): wait out HTTP 429 and retry; add RELEASING.md.** The crates.io new-crate rate limit no longer fails the run: it waits ~11 minutes and retries the same crate. RELEASING.md records that crates.io and PyPI are published and npm is on hold due to npm-side bugs, with the steps to resume. |
| `804e641` | **feat(vad): move inference from ONNX Runtime to pure-Rust tract,** removing the Windows runtime DLL dependency. Uses tract-onnx 0.23.7 and upstream silero-vad's If-free 16 kHz model; outputs are resolved by name at load time (fail-closed); 8 kHz input is resampled to 16 kHz with rubato. Guard tests pin the probabilities to the previous ort implementation (within 1e-4). The Windows release build checks the `.node` has no MSVC/DirectML/onnxruntime DLL dependency. flexaudio-vad's MSRV becomes 1.91. |
| `ab8df7f` | **ci(release-npm): replace dumpbin with our own PE parser** for the Windows dependency check. dumpbin is not on the runner's default PATH, so the check failed and skipped the artifact upload even though the build succeeded. `check-pe-dependencies.mjs` reads regular and delay imports with Node built-ins and fails closed on unreadable input. |
| `b2375d3` | **fix(vad): address Rust 1.98's new lint `chunks_exact_to_as_chunks`.** Four test-only `chunks_exact(N)` calls become `as_chunks::<N>().0` (same behaviour). CI had turned red with no code change because `@stable` moved to 1.98. |
| `0e6cbea` | **ci: pin the gates' toolchain to 1.98.1 (#2)** and add `toolchain-pin-guard`, an advance-warning job on upstream stable, native clippy on Windows/macOS, and MSRV gates for every crate declaring 1.85 or 1.91. |
| `e36ca9f` | **fix(mic): create cpal's WASAPI enumerator on a long-lived keeper thread (Windows).** cpal 0.16 creates a process-wide enumerator in the STA of the first thread that touches it and uninitializes COM when that thread exits, so later cpal calls crashed with 0xC0000005 (reachable via `switchSource` to mic/mix, then `stop`). All cpal entry points now go through one gate; flexaudio-mic is back in the Windows CI tests. |
| `d4eef38` | **fix(macos): do not turn a device-enumeration failure into "zero devices".** `all_device_ids` now returns a `Result` and propagates the OSStatus through the crate's single mapping path, `map_os_status`, instead of `unwrap_or_default()`. |
| `f6a30e7` | **fix(stream): do not lose the DISCONTINUITY flag on the first chunk after `resume()`.** The resume/intake race is closed by deciding the flag inside the delivery lock with a resume generation counter, per tap. |
| `846d7d9` | **fix(macos): move the `Error` import into the test module** (it was unused in non-test code and failed macOS clippy `-D warnings`). |
| `480433e` | **fix(napi): let the PE dependency check read `libc++.dll`** (t197). The DLL-name character class lacked `+`, so every llvm-mingw PE was rejected. The rule now lives only in `pe-dll-name.mjs`, with node:test coverage. |
| `55417c4` | **fix(napi): turn the PE dependency check into an allow list** (t314). Any dependency outside the allow set (measured from the shipped `.node` files, plus API sets) now fails, and forbidden patterns still take precedence. Adds end-to-end tests on PEs assembled in the test. |
