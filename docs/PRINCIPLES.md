# Target state (design principles)

This is the maintainer's canonical account of what flexaudio promises and does
not promise, not a specification or a requirements list. The API is the
specification; this is the target it must serve.

flexaudio is desktop capture infrastructure across languages: nine capture
cells, fixed framing, explicit differences and side effects, one processing
path, and typed failures. Applications should record by reading the library's
answers, without branching on OS names.

It is an MIT-licensed Rust library for Linux, Windows and macOS, including native
system loopback, distributed through crates.io, npm, PyPI and a C ABI. It returns
normalized `f32` chunks and typed events, with fixed 20 ms framing and an internal
48 kHz stereo format. VAD, denoise, encoding and a CLI accompany capture. Runtime
network access is unnecessary; the VAD model is embedded.

## Six promises

1. **A foundation for desktop recording.** The nine cells are three desktop
   OSes × microphone, system output and process capture. `SourceKind::Mix`
   combines microphone and system output on all three; it is not a tenth cell.
   Normalization and the capture contract are shared by Rust, Node.js, Python
   and C.
2. **Differences are explicit.** Read `capabilities()` before opening, then the
   effective settings report and explicit errors. Capabilities describe support,
   not permission. Read `process.include_scope`, `process.exclude_scope` and
   `system_exclusions.scope` for `PidScope::ExactPid` or
   `PidScope::ProcessTree` (or `PidScope::NotApplicable`), rather than branching
   on OS names.
3. **Side effects are visible; defaults favor detecting failure.** Missing a
   failure is worse than producing a sound. The macOS `silence_probe` stays on
   by default, including in 1.0: it diagnoses missing system-audio permission
   that can otherwise yield only zero chunks without an error or event. Its
   signal may be audible and may enter the capture. Its lifecycle is exposed as
   `Event::SilenceProbeStarted` and `Event::SilenceProbeStopped` and documented.
   Separately, Windows `OpenOptions::windows_classic_loopback_keepalive` stays
   on by default: its inaudible silent rendering is needed to keep idle classic
   loopback delivering correctly. It reports
   `Event::ClassicLoopbackKeepaliveStarted` and
   `Event::ClassicLoopbackKeepaliveStopped`.
4. **One processing path across languages.** Denoise, settings applicability,
   VAD and secondary output share processing semantics; bindings adapt the
   interface rather than implement separate decisions.
5. **Failures remain visible and typed.** Capture and cleanup failures must not
   disappear into silence; retained errors and `ShutdownReport` expose them.
   Equal failure values may be kept once in `ShutdownReport`. The caller still
   receives the failure. Failures that do not compare equal are kept separately.
6. **Versions keep their promises.** Patches fix bugs and documentation; minors
   add or change features. The default and compatibility changes below are made
   together in 0.9, not scattered across minors. Only the removal of `pts_ns`
   waits for 0.10, because the old names were published to remain through 0.9.

## The road to 1.0

0.9 collects the default and compatibility changes:

- Strict mode becomes the default, so opening rejects requested settings that
  would be ignored. Before 0.9, `OpenOptions::strict` defaults to `false`.
- Omitted `denoise` on Python `switch_source` has the same meaning as in the
  other bindings.
- A new C configuration entry point, v5, treats an explicit `gain = 0` as mute.
  The existing C entry points keep interpreting `gain = 0` as unity gain (1.0).
- `AudioChunk`, `DeviceInfo` and `StreamConfig` are marked `#[non_exhaustive]`.
  Constructor functions are added at the same time, so users do not have to
  write these structs field by field.
- Denoise works at any output rate.
- Promises 1 and 5 get capture-cell, binding error-kind, shutdown-report field
  and discarded-cleanup checks.

0.10 removes `pts_ns` / `ptsNs` in favor of `arrival_ns` / `arrivalNs`. The old
names remain through 0.9, as published.

1.0 only declares stability. It adds no new breaking change.

## How progress is checked

Progress means closing the distance to these six promises, not fixing a score.
Promise 1 has `current_os_capture_cells_have_capabilities_and_construction_paths`:
on each desktop OS, the public capabilities and real backend constructors must
support microphone, system output and process capture. It never starts capture;
device and permission errors are acceptable, unsupported errors are not. An
unavailable capability fails, and failure messages include the runner OS version.
Promise 2 has the [binding capability matrix CI check](../.github/scripts/binding_matrix.py);
it checks declarations, not runtime behavior. Promise 3 has the backend probe
and keepalive lifecycle tests. Promise 4 has
`bindings_cannot_construct_applicability_reasons`: bindings may not name
`IgnoreReason` or `AdjustmentReason`, including their variants. Promise 5 has
runtime binding checks against the flexaudio-core error-kind inventory: every
kind maps to its canonical name and no two names collide. This is not a
source-text check. Shutdown field checks compare an exhaustive core
`ShutdownReport` destructure with the Node, Python and current C declarations.

A stop or cleanup failure that the library observes stays in `ShutdownReport`
and reaches the caller in Rust, Node, Python and C. CI injects operation failures
into Stream, Mix, mock backends, microphone and each OS backend. Shared fixtures
run on Linux, Windows and macOS; PipeWire system/process/watcher fixtures run on
Linux, WASAPI system/process fixtures on Windows, and Core Audio
system/process/tap/probe cleanup fixtures on macOS. Binding caller tests run in
the Linux jobs. Cross-crate fixtures use non-default `test-support` features
enabled only by dev-dependencies; they are internal and have no stability guarantee.

Two regressions turn CI red automatically. A new stop path in `StopPath` needs
a named arm in a wildcard-free match (compiler error E0004) and exactly one
fixture registration. One registration list generates both `RegisteredBuiltin`
implementations and the executable test inventory. Each registration is bound
to a fixture that observes the same concrete backend type: a wrong type/path
pair fails to compile. A built-in backend passed through the registered factory
must implement `RegisteredBuiltin` (E0277). Stream is tested directly because
it is not a factory backend. Each phase enum generates its complete test
inventory from its definition. A discarded `Result` written as `let _ =` or
`let _: T =` fails `clippy::let_underscore_must_use` in the eight production
crates: flexaudio, flexaudio-mic, the three OS backends, flexaudio-napi,
flexaudio-py and flexaudio-ffi. Intentional non-stop discards carry a local
reason; stop and cleanup paths retain their outcomes without lint exceptions.
The registration proves that each path has a typed fixture that touches its
backend, not what the fixture asserts. Fixture contents are reviewed like any
other test.

The limit is explicit: a failure swallowed inside a function in some other way,
outside the registered entry and without a discarded `Result`, is not detected
automatically. These checks replace the removed source-text stop guard.

Promise 6 has
the API compatibility gates and the
[registry completion gate](../.github/scripts/check_registry_completion.py).
These checks guard parts of the target; they do not prove it complete or verify
that audio is recorded.

The complete target is an application that reads capabilities at startup and
the report when opening, uses defaults that reject ineffective settings,
receives side-effect start/stop events and typed failures, and keeps the same
processing meaning when changing languages, without OS-name branches.

## Outside the promise

OS differences do not disappear. Old OSes, PulseAudio-only system/process
capture, mobile, a playback API, a mixer, multichannel audio, arbitrary latency,
exclusive ultra-low latency, DAWs and a universal audio framework are outside
the scope. Moving Linux microphone capture to PipeWire is not a completion
condition: device ID namespaces already let applications avoid OS branches.
Stars, adoption and maintainer counts are not completion criteria either.
