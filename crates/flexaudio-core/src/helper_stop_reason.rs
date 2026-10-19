//! Reasons a capture helper relinquished ownership after teardown attempts.

/// Why a diagnostic probe or silent loopback renderer stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum HelperStopReason {
    /// The diagnostic signal was recognized.
    SignalObserved,
    /// Recording permission was denied.
    PermissionDenied,
    /// The diagnostic evidence could not establish an outcome.
    Inconclusive,
    /// The diagnostic deadline expired.
    TimedOut,
    /// Capture was stopped explicitly.
    StreamStopped,
    /// Capture is switching to another source.
    SourceSwitched,
    /// Capture is restarting for recovery.
    Recovery,
    /// Helper or capture startup failed.
    StartupFailed,
    /// Running the helper or attempting teardown failed.
    RuntimeFailed,
    /// The attempt was cancelled independently of capture shutdown.
    Cancelled,
}

impl HelperStopReason {
    /// Stable lower-camel tag shared by language bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignalObserved => "signalObserved",
            Self::PermissionDenied => "permissionDenied",
            Self::Inconclusive => "inconclusive",
            Self::TimedOut => "timedOut",
            Self::StreamStopped => "streamStopped",
            Self::SourceSwitched => "sourceSwitched",
            Self::Recovery => "recovery",
            Self::StartupFailed => "startupFailed",
            Self::RuntimeFailed => "runtimeFailed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HelperStopReason::*;

    #[test]
    fn reason_tags_follow_the_public_contract() {
        let reasons = [
            SignalObserved,
            PermissionDenied,
            Inconclusive,
            TimedOut,
            StreamStopped,
            SourceSwitched,
            Recovery,
            StartupFailed,
            RuntimeFailed,
            Cancelled,
        ];
        assert_eq!(
            reasons.map(|reason| reason.as_str()),
            [
                "signalObserved",
                "permissionDenied",
                "inconclusive",
                "timedOut",
                "streamStopped",
                "sourceSwitched",
                "recovery",
                "startupFailed",
                "runtimeFailed",
                "cancelled"
            ]
        );
    }
}
