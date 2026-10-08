//! Synchronous, scoped macOS own-output permission probe. Never called from an IOProc.
//!
//! Deadlines/cancellation are cooperative between native calls, including setup and teardown.
//! Core Audio's synchronous calls cannot be preempted if the OS itself hangs; no detached worker
//! disguises that limitation. Both callback registrations own copied blocks with Arc-owned data.

mod capture;
mod io;
mod output;

use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use objc2_core_audio::kAudioProcessPropertyIsRunningOutput;
use objc2_foundation::NSUUID;

use crate::common::translate_pid_to_object;
use crate::probe::{Probe, ProbeControl, ProbeOutcome};
use crate::probe_signal::{identify, observation_end, ProbeSignal, SignalEvidence};
use crate::processes::read_u32_property;

use capture::Capture;
use io::{check, failed};
use output::{default_device, Output};

#[repr(C)]
struct HostTimebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    // Public libSystem/Mach API. Declare the C POD locally because libc's transitional
    // mach_timebase_info aliases are deprecated, despite this underlying native API.
    fn mach_timebase_info(info: *mut HostTimebase) -> i32;
    fn mach_absolute_time() -> u64;
}

pub(crate) struct NativeProbe;

impl NativeProbe {
    pub(crate) fn new() -> Self {
        Self
    }

    fn execute(control: &ProbeControl) -> Result<ProbeOutcome, ProbeOutcome> {
        check(control)?;
        let mut timebase = HostTimebase { numer: 0, denom: 0 };
        // SAFETY: timebase is valid writable storage for the public Mach host-time conversion.
        let status = unsafe { mach_timebase_info(&mut timebase) };
        if status != 0 || timebase.numer == 0 || timebase.denom == 0 {
            return Err(failed("Mach host clock conversion", status));
        }
        let ticks_per_second =
            1_000_000_000.0 * f64::from(timebase.denom) / f64::from(timebase.numer);
        // UUID is only a collision-resistant per-attempt waveform nonce, never logged or persisted.
        let nonce = NSUUID::new()
            .UUIDString()
            .to_string()
            .bytes()
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        let signal = ProbeSignal::new(nonce);
        let mut output = Output::new(&signal, ticks_per_second, control)?;
        // A process without prior playback may acquire its audio object only after silent IO starts.
        let own_pid =
            i32::try_from(std::process::id()).map_err(|_| failed("host PID range", -1))?;
        let own_object = loop {
            check(control)?;
            match translate_pid_to_object(own_pid) {
                Ok(0) => thread::sleep(Duration::from_millis(5)),
                Ok(object) => break object,
                Err(error) => {
                    return Err(ProbeOutcome::Failed {
                        detail: format!("self-probe host process lookup: {error}"),
                    })
                }
            }
        };
        let mut capture = Capture::new(
            own_object,
            ticks_per_second,
            output.state.enabled.clone(),
            control,
        )?;
        check(control)?;
        output.state.enabled.store(true, Ordering::Release);
        let mut output_active = false;
        loop {
            check(control)?;
            if output.state.invalid.load(Ordering::Acquire)
                || capture.state.invalid.load(Ordering::Acquire)
            {
                return Err(failed("invalid or discontinuous callback evidence", -1));
            }
            if output.state.finished() {
                break;
            }
            // Output-content evidence is primary; this property additionally verifies live output I/O.
            match read_u32_property(own_object, kAudioProcessPropertyIsRunningOutput) {
                Some(true_value) if true_value != 0 => output_active = true,
                Some(_) => {}
                None => return Err(failed("output-running property unavailable", -1)),
            }
            thread::sleep(Duration::from_millis(5));
        }
        let rendered = output.state.evidence(capture.state.rate, false);
        let observation_target = observation_end(&rendered)
            .ok_or_else(|| failed("invalid render-plus-latency host-clock interval", -1))?;
        // Callback submission timestamps can precede actual rendering. Keep both the live
        // capture and the real Mach clock through the last rendered frame plus 500 ms.
        // Missing/stopped capture times out inconclusively, even if its initial frames were zero.
        loop {
            check(control)?;
            if output.state.invalid.load(Ordering::Acquire)
                || capture.state.invalid.load(Ordering::Acquire)
            {
                return Err(failed("invalid or discontinuous callback evidence", -1));
            }
            // SAFETY: public Mach clock read takes no pointers and shares Core Audio host-time units.
            let now = unsafe { mach_absolute_time() };
            if now >= observation_target
                && capture
                    .state
                    .observed_end()
                    .is_some_and(|end| end >= observation_target)
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        check(control)?;
        if default_device()? != output.device {
            return Err(failed("default output changed during probe", -1));
        }
        let output_stopped = output.stop();
        let capture_stopped = capture.close();
        let valid = output_stopped
            && capture_stopped
            && output_active
            && !capture.state.invalid.load(Ordering::Acquire);
        let evidence = output.state.evidence(capture.state.rate, valid);
        let frames = capture.state.snapshot();
        // Teardown must be complete before any inference is returned to the generation policy.
        drop(capture);
        drop(output);
        if control.cancelled() {
            return Ok(ProbeOutcome::Cancelled);
        }
        if control.timed_out() {
            return Ok(ProbeOutcome::TimedOut);
        }
        let identified = identify(&signal, &evidence, &frames);
        if control.cancelled() {
            return Ok(ProbeOutcome::Cancelled);
        }
        if control.timed_out() {
            return Ok(ProbeOutcome::TimedOut);
        }
        Ok(match identified {
            SignalEvidence::Matched => ProbeOutcome::Present,
            SignalEvidence::ExactZeros => ProbeOutcome::Denied { detail: "The private self-probe captured continuous exact-zero samples covering at least 95% of the verified default-output diagnostic signal and its complete 500 ms capture-latency window; System Audio Recording access was not granted".into() },
            SignalEvidence::Inconclusive => ProbeOutcome::Failed { detail: "The private self-probe did not obtain an unambiguous matching signal or continuous overlapping zero capture with verified rendering".into() },
        })
    }
}

impl Probe for NativeProbe {
    fn run(&mut self, control: &ProbeControl) -> ProbeOutcome {
        // The backend owner is a Rust thread without an ambient Foundation pool.
        // Drain convenience-method temporaries only after all probe owners finish teardown.
        objc2::rc::autoreleasepool(|_| match Self::execute(control) {
            Ok(outcome) => outcome,
            Err(_) if control.cancelled() => ProbeOutcome::Cancelled,
            Err(_) if control.timed_out() => ProbeOutcome::TimedOut,
            Err(outcome) => outcome,
        })
    }
}
