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
6. **Versions keep their promises.** Patches fix bugs and documentation; minors
   add or change features. Default and compatibility changes below belong
   together in 1.0, not scattered across minors.

## The 1.0 boundary

Strict mode becomes the default, so opening rejects requested settings that
would be ignored. Today `OpenOptions::strict` defaults to `false`. In the same
release: remove `pts_ns` / `ptsNs` in favor of `arrival_ns` / `arrivalNs` (the old
names remain through 0.9); align omitted `denoise` on Python `switch_source`
with the other bindings; stop interpreting C configuration `gain=0` as unity;
mark `AudioChunk`, `DeviceInfo` and `StreamConfig` `#[non_exhaustive]`.

## How progress is checked

Progress means closing the distance to these six promises, not fixing a score.
Promise 2 has the [binding capability matrix CI check](../.github/scripts/binding_matrix.py);
it checks declarations, not runtime behavior. Promise 3 has the backend probe
and keepalive lifecycle tests. Promise 4 has
`bindings_cannot_construct_applicability_reasons`: bindings may not name
`IgnoreReason` or `AdjustmentReason`, including their variants. Promise 6 has
the API compatibility gates and the
[registry completion gate](../.github/scripts/check_registry_completion.py).
Promises 1 and 5 do not yet have a check covering the whole promise. These
checks guard parts of the target; they do not prove it complete.

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
