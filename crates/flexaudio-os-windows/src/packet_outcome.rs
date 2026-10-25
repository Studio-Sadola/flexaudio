//! Retain packet processing and release outcomes in observation order.

use flexaudio_core::terminal_causes::retain_terminal_cause;
use flexaudio_core::Result;

pub(crate) fn combine_packet_outcomes(packet: Result<()>, release: Result<()>) -> Result<()> {
    let mut retained = packet.err();
    if let Err(error) = release {
        retain_terminal_cause(&mut retained, error);
    }
    retained.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::Error;

    #[test]
    fn packet_failure_remains_primary_when_release_also_fails() {
        flexaudio_core::test_support::assert_distinct_failures_retained(
            Error::Backend("packet validation failed".into()),
            Error::Backend("packet release failed".into()),
            |packet, release| combine_packet_outcomes(Err(packet), Err(release)).unwrap_err(),
        );
    }

    #[test]
    fn clean_and_single_failure_outcomes_remain_unchanged() {
        assert_eq!(combine_packet_outcomes(Ok(()), Ok(())), Ok(()));
        for (packet, release) in [
            (Err(Error::DeviceLost), Ok(())),
            (Ok(()), Err(Error::DeviceLost)),
            (Err(Error::DeviceLost), Err(Error::DeviceLost)),
        ] {
            assert_eq!(
                combine_packet_outcomes(packet, release),
                Err(Error::DeviceLost)
            );
        }
    }
}
