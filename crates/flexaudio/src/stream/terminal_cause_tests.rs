//! A later terminal cause stays in the retained report beside the first one.
use super::*;
use std::time::Instant;

struct TwoFailures {
    events: VecDeque<Event>,
    final_events: VecDeque<Event>,
    stop_result: Result<()>,
}

impl CaptureBackend for TwoFailures {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _sink: RawSink) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self) {
        self.events.append(&mut self.final_events);
    }
    fn stop_checked(&mut self) -> Result<()> {
        self.stop();
        self.stop_result.clone()
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

#[derive(Clone, Copy, Debug)]
enum Arrival {
    /// The second failure is already queued behind the first.
    Queued,
    /// The second failure appears only in the final stop drain.
    FinalDrain,
}

fn terminal(error: &Error) -> Event {
    Event::TerminalError {
        error: error.clone(),
    }
}

fn run(arrival: Arrival, wait_for_watchdog: bool, second: Error) -> (Stream, Result<()>) {
    let first = Error::DeviceLost;
    let mut backend = TwoFailures {
        events: VecDeque::from([terminal(&first)]),
        final_events: VecDeque::new(),
        stop_result: Ok(()),
    };
    match arrival {
        Arrival::Queued => backend.events.push_back(terminal(&second)),
        Arrival::FinalDrain => backend.final_events.push_back(terminal(&second)),
    }
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    if wait_for_watchdog {
        let deadline = Instant::now() + Duration::from_secs(5);
        while stream.terminal_error().is_none() {
            assert!(
                Instant::now() < deadline,
                "watchdog did not observe failure"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    let result = stream.stop_checked();
    (stream, result)
}

fn assert_one_terminal_event(stream: &mut Stream) {
    let mut terminal_events = Vec::new();
    while let Some(event) = stream.poll_event() {
        match event {
            Event::TerminalError { error } => terminal_events.push(error),
            Event::ShutdownError { error } => panic!("terminal cause became cleanup: {error:?}"),
            _ => {}
        }
    }
    assert_eq!(terminal_events, vec![Error::DeviceLost]);
}

#[test]
fn later_distinct_terminal_cause_is_retained_with_one_terminal_event() {
    let second = Error::Backend("b".into());
    let expected = Error::Multiple(ErrorGroup::new(
        Error::DeviceLost,
        second.clone(),
        Vec::new(),
    ));
    for arrival in [Arrival::Queued, Arrival::FinalDrain] {
        for wait_for_watchdog in [false, true] {
            let (mut stream, result) = run(arrival, wait_for_watchdog, second.clone());
            assert_eq!(result, Err(expected.clone()), "{arrival:?}");
            let report = stream.shutdown_report().unwrap();
            assert_eq!(report.primary(), Some(&expected));
            assert!(report.cleanup().is_empty());
            assert_eq!(stream.terminal_error(), Some(expected.clone()));
            assert_eq!(stream.stop_checked(), Err(expected.clone()));
            assert_one_terminal_event(&mut stream);
        }
    }
}

#[test]
fn later_equal_terminal_cause_is_kept_once() {
    for arrival in [Arrival::Queued, Arrival::FinalDrain] {
        for wait_for_watchdog in [false, true] {
            let (mut stream, result) = run(arrival, wait_for_watchdog, Error::DeviceLost);
            assert_eq!(result, Err(Error::DeviceLost), "{arrival:?}");
            let report = stream.shutdown_report().unwrap();
            assert_eq!(report.primary(), Some(&Error::DeviceLost));
            assert!(report.cleanup().is_empty());
            assert_one_terminal_event(&mut stream);
        }
    }
}

/// A backend whose checked stop also returns both retained causes (as Mix does)
/// must not turn either of them into cleanup.
#[test]
fn grouped_stop_result_with_both_terminal_causes_adds_no_cleanup() {
    let second = Error::Backend("b".into());
    let expected = Error::Multiple(ErrorGroup::new(
        Error::DeviceLost,
        second.clone(),
        Vec::new(),
    ));
    let backend = TwoFailures {
        events: VecDeque::from([terminal(&Error::DeviceLost), terminal(&second)]),
        final_events: VecDeque::new(),
        stop_result: Err(expected.clone()),
    };
    let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
    stream.start().unwrap();
    assert_eq!(stream.stop_checked(), Err(expected.clone()));
    let report = stream.shutdown_report().unwrap();
    assert_eq!(report.primary(), Some(&expected));
    assert!(report.cleanup().is_empty());
    assert_one_terminal_event(&mut stream);
}

#[test]
fn distinct_legacy_payloads_survive_final_drain() {
    flexaudio_core::test_support::assert_distinct_failures_retained(
        flexaudio_core::terminal_causes::legacy_capture_error("first legacy failure".into()),
        flexaudio_core::terminal_causes::legacy_capture_error("second legacy failure".into()),
        |_, _| {
            let backend = TwoFailures {
                events: VecDeque::from([Event::Error("first legacy failure".into())]),
                final_events: VecDeque::from([Event::Error("second legacy failure".into())]),
                stop_result: Ok(()),
            };
            let mut stream = Stream::open(StreamConfig::default(), Box::new(backend)).unwrap();
            stream.start().unwrap();
            stream.stop_checked().unwrap_err()
        },
    );
}
