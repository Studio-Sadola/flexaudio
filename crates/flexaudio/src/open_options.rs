//! Capture-open settings that do not change the audio stream configuration.

/// Optional backend behavior selected when opening or switching a stream.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOptions {
    /// Reject non-default capture settings that would be ignored, before native access.
    /// Defaults to false; plain source switches retain the current value.
    pub strict: bool,
    /// Keep Windows classic system loopback active by rendering silence.
    ///
    /// Enabled by default. Applies to system capture and the system lane of a mix
    /// without process exclusions. Other operating systems and process-loopback
    /// paths ignore this setting.
    ///
    /// When disabled, an idle endpoint delivers no packets. The unchanged 2 s
    /// watchdog reports [`crate::Event::StreamStalled`] and reopens repeatedly
    /// until audio plays.
    pub windows_classic_loopback_keepalive: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            strict: false,
            windows_classic_loopback_keepalive: true,
        }
    }
}

impl OpenOptions {
    /// Enable or disable strict applicability validation.
    pub fn strict(mut self, enabled: bool) -> Self {
        self.strict = enabled;
        self
    }
    /// Create the default options, including Windows classic loopback keepalive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable or disable Windows classic loopback's silent-render keepalive.
    pub fn windows_classic_loopback_keepalive(mut self, enabled: bool) -> Self {
        self.windows_classic_loopback_keepalive = enabled;
        self
    }
}
