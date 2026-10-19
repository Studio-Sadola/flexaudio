//! Separate, ordered helper lifecycle JSON output.
use flexaudio::Event;
use serde_json::{json, Value};
use std::io::{self, Write};

/// Consume helper events even when delivery is disabled, keeping diagnostics separate.
pub(super) fn route(event: &Event, enabled: bool, output: &mut impl Write) -> io::Result<bool> {
    let Some(line) = project(event) else {
        return Ok(false);
    };
    if enabled {
        writeln!(output, "{line}")?;
    }
    Ok(true)
}

pub(super) fn project(event: &Event) -> Option<Value> {
    Some(match event {
        Event::SilenceProbeStarted => json!({"type": "silenceProbeStarted"}),
        Event::SilenceProbeStopped { reason } => {
            json!({"type": "silenceProbeStopped", "reason": reason.as_str()})
        }
        Event::ClassicLoopbackKeepaliveStarted => {
            json!({"type": "classicLoopbackKeepaliveStarted"})
        }
        Event::ClassicLoopbackKeepaliveStopped { reason } => {
            json!({"type": "classicLoopbackKeepaliveStopped", "reason": reason.as_str()})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::HelperStopReason;

    #[test]
    fn lifecycle_lines_have_shared_tags_and_no_reason_on_start() {
        let events = [
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::SourceSwitched,
            },
            Event::ClassicLoopbackKeepaliveStarted,
            Event::ClassicLoopbackKeepaliveStopped {
                reason: HelperStopReason::StreamStopped,
            },
        ];
        let lines: Vec<_> = events
            .iter()
            .map(|event| project(event).unwrap().to_string())
            .collect();
        let values: Vec<Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            values,
            vec![
                json!({"type":"silenceProbeStarted"}),
                json!({"type":"silenceProbeStopped","reason":"sourceSwitched"}),
                json!({"type":"classicLoopbackKeepaliveStarted"}),
                json!({"type":"classicLoopbackKeepaliveStopped","reason":"streamStopped"}),
            ]
        );
        assert!(project(&Event::StreamStalled).is_none());
    }

    #[test]
    fn routing_is_ordered_and_silent_when_disabled() {
        let events = [
            Event::SilenceProbeStarted,
            Event::SilenceProbeStopped {
                reason: HelperStopReason::Cancelled,
            },
            Event::ClassicLoopbackKeepaliveStarted,
            Event::ClassicLoopbackKeepaliveStopped {
                reason: HelperStopReason::StartupFailed,
            },
        ];
        let mut output = Vec::new();
        for event in &events {
            assert!(route(event, false, &mut output).unwrap());
        }
        assert!(output.is_empty());
        for event in &events {
            assert!(route(event, true, &mut output).unwrap());
        }
        assert!(!route(&Event::StreamStalled, true, &mut output).unwrap());
        let text = String::from_utf8(output).unwrap();
        let actual: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected: Vec<Value> = events.iter().map(|event| project(event).unwrap()).collect();
        assert_eq!(actual, expected);
    }
}
