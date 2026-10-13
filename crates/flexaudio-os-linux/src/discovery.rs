// Failure steps for authoritative device inventory queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum EnumerationFailure {
    SecondSync,
    CoreGone,
    Iterate,
    Deadline,
    Identity,
    Format,
    Metadata,
    Callback,
}
impl EnumerationFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::SecondSync => "pipewire second registry sync failed",
            Self::CoreGone => "pipewire core disappeared during registry enumeration",
            Self::Iterate => "pipewire registry loop iteration failed",
            Self::Deadline => "pipewire registry enumeration timed out",
            Self::Identity => "pipewire device has no mandatory stable identity",
            Self::Format => "pipewire device advertised an invalid native format",
            Self::Metadata => "pipewire default metadata query failed",
            Self::Callback => "pipewire registry query callback rejected",
        }
    }
}

impl EnumerationFailure {
    pub(crate) fn error(self) -> flexaudio_core::Error {
        flexaudio_core::Error::Backend(format!(
            "{}; check the audio session and retry the query",
            self.message()
        ))
        .with_context(flexaudio_core::ErrorContext::new(
            flexaudio_core::Operation::Enumerate,
        ))
    }
}

// Daemon text is untrusted private diagnostic data: preserve it through the
// structured opt-in carrier, never interpolate it into Display or logging.
pub(crate) fn core_error(object_id: u32, code: i32, message: &str) -> flexaudio_core::Error {
    flexaudio_core::Error::Backend(format!(
        "PipeWire object {object_id} query failed with status {code} ({}); check audio-session access and retry the query",
        std::io::Error::from_raw_os_error(code.saturating_neg())
    )).with_context(flexaudio_core::ErrorContext::new(flexaudio_core::Operation::Enumerate))
        .with_diagnostic_detail(message.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn daemon_diagnostic_retains_private_cause_only_in_structured_access() {
        let error = super::core_error(7, -5, "synthetic private daemon context");
        assert_eq!(error.kind(), flexaudio_core::ErrorKind::Backend);
        assert_eq!(
            error.diagnostic_details(),
            ["synthetic private daemon context"]
        );
        assert!(error.to_string().contains("-5"));
        assert!(error.to_string().contains("retry"));
        assert!(!error.to_string().contains("synthetic private"));
    }
}

// Absent registry properties retain the established requested-format defaults.
// Explicitly invalid properties are unreadable mandatory values, never plausible defaults.
pub(crate) fn device_format(rate: Option<&str>, channels: Option<&str>) -> (u32, u16, bool) {
    let query = || {
        let rate = rate
            .map_or(Ok(48_000), str::parse::<u32>)
            .map_err(|_| EnumerationFailure::Format.error())?;
        let channels = channels
            .map_or(Ok(2), str::parse::<u16>)
            .map_err(|_| EnumerationFailure::Format.error())?;
        Ok((rate, channels))
    };
    flexaudio_core::inventory::device_format(query())
}

#[cfg(test)]
mod round3_tests {
    use super::*;
    #[test]
    fn invalid_properties_keep_inventory_neighbors_without_fabricated_formats() {
        let formats = flexaudio_core::inventory::collect(
            Ok([
                (Some("44100"), Some("6")),
                (Some("broken"), Some("2")),
                (None, None),
                (Some("48000"), Some("0")),
            ]),
            |(rate, channels)| Some(device_format(rate, channels)),
        )
        .unwrap();
        assert_eq!(
            formats,
            [
                (44_100, 6, true),
                (0, 0, false),
                (48_000, 2, true),
                (0, 0, false)
            ]
        );
    }
}
