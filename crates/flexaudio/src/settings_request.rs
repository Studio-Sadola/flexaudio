//! Decoded, device-independent inputs to settings resolution.

use crate::{OpenOptions, SettingKey, SettingsReport, StreamConfig};

/// Settings outside [`StreamConfig`] and front-end switch policy for a capture request.
///
/// This describes configured attachment presence, never model paths or model loading.
/// It does not enable processing itself; the attachment owner still installs it.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsAttachments {
    pub(crate) keepalive: Option<Option<bool>>,
    pub(crate) denoise: Option<bool>,
    pub(crate) vad: Option<bool>,
    pub(crate) secondary_encoding: Option<String>,
    pub(crate) fixed_at_open: Vec<SettingKey>,
    pub(crate) retained_on_switch: Vec<SettingKey>,
    pub(crate) omitted_on_switch: Vec<SettingKey>,
    pub(crate) legacy_sentinels: Vec<SettingKey>,
}

impl SettingsAttachments {
    /// Describe an explicit or omitted keepalive request separately from the applied
    /// backend value. `None` requests the default and retains the switch value.
    pub fn windows_classic_loopback_keepalive(mut self, enabled: Option<bool>) -> Self {
        self.keepalive = Some(enabled);
        self
    }
    /// Describe no attachments. Omitted attachments retain their values on switch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Describe whether canonical capture denoising is attached.
    pub fn denoise(mut self, enabled: bool) -> Self {
        self.denoise = Some(enabled);
        self
    }

    /// Describe VAD attachment presence without exposing its model or runtime options.
    pub fn vad(mut self, present: bool) -> Self {
        self.vad = Some(present);
        self
    }

    /// Describe the secondary tap's decoded encoding tag, whose default is `f32`.
    pub fn secondary_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.secondary_encoding = Some(encoding.into());
        self
    }

    /// Retain these settings on switch and classify non-default requests as ignored.
    /// Gain and ring capacity are always fixed at open, independently of this list.
    pub fn fixed_at_open(mut self, keys: impl IntoIterator<Item = SettingKey>) -> Self {
        self.fixed_at_open = keys.into_iter().collect();
        self
    }

    /// Retain these settings' effective values on switch, without making them ignored.
    /// Use this for attachments that a front end cannot replace during a source switch.
    /// Omit their request values to report the documented default as `NotRequested`.
    pub fn retained_on_switch(mut self, keys: impl IntoIterator<Item = SettingKey>) -> Self {
        self.retained_on_switch = keys.into_iter().collect();
        self
    }

    /// Describe settings without switch arguments: request their documented defaults
    /// and retain the previous effective values. Fixed-at-open rules still apply.
    pub fn omitted_on_switch(mut self, keys: impl IntoIterator<Item = SettingKey>) -> Self {
        self.omitted_on_switch = keys.into_iter().collect();
        self
    }

    /// Identify settings decoded from legacy sentinels before this request was built.
    /// Applicable non-default values are adjusted; defaults remain `NotRequested`.
    pub fn legacy_sentinels(mut self, keys: impl IntoIterator<Item = SettingKey>) -> Self {
        self.legacy_sentinels = keys.into_iter().collect();
        self
    }
}

/// A decoded capture request for pure settings preflight, before native or model work.
///
/// Requests are compared with documented defaults, including on source switches.
/// Supply the last committed report with [`Self::previous`] for switch revisions and
/// retained values. Resolution does not validate capture arguments or load attachments.
///
/// ```
/// use flexaudio::{resolve_settings, SettingKey, SettingsAttachments, SettingsRequest, StreamConfig};
/// let request = SettingsRequest::new(StreamConfig::default()).attachments(
///     SettingsAttachments::new().denoise(true).fixed_at_open([SettingKey::Denoise]),
/// );
/// let opened = resolve_settings(&request)?;
/// let switched = resolve_settings(&request.previous(&opened))?;
/// assert!(switched.has_ignored());
/// # Ok::<(), flexaudio::Error>(())
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRequest {
    pub(crate) config: StreamConfig,
    pub(crate) keepalive: Option<bool>,
    // The backend control survives source switches even when the report's
    // source-filtered effective value is absent. Keep it separate from the
    // decoded request, which may be omitted while this value remains false.
    pub(crate) configured_keepalive: Option<bool>,
    pub(crate) attachments: SettingsAttachments,
    pub(crate) previous: Option<SettingsReport>,
}

impl SettingsRequest {
    /// Start an open request using documented defaults for non-config settings.
    pub fn new(config: StreamConfig) -> Self {
        Self {
            config,
            keepalive: None,
            configured_keepalive: None,
            attachments: SettingsAttachments::default(),
            previous: None,
        }
    }

    /// Describe attachments and front-end switch policy.
    pub fn attachments(mut self, attachments: SettingsAttachments) -> Self {
        if let Some(keepalive) = attachments.keepalive {
            self.keepalive = keepalive;
        }
        self.attachments = attachments;
        self
    }

    /// Use the attachment description and keepalive value supplied to native open.
    /// Strictness is enforced by the caller after inspecting the resolved report.
    pub fn open_options(mut self, options: &OpenOptions) -> Self {
        self.configured_keepalive = Some(options.windows_classic_loopback_keepalive);
        self.keepalive = options
            .settings_attachments
            .keepalive
            .unwrap_or(Some(options.windows_classic_loopback_keepalive));
        self.attachments = options.settings_attachments.clone();
        self
    }

    /// Set the decoded keepalive request. `None` means default on open and retained
    /// on switch; an omitted switch request still records the default value `true`.
    pub fn windows_classic_loopback_keepalive(mut self, enabled: Option<bool>) -> Self {
        self.keepalive = enabled;
        self
    }

    /// Select switch semantics using the last committed settings snapshot.
    /// Supply retained native controls with [`Self::open_options`]: a report's
    /// inapplicable effective values do not preserve those configured controls.
    /// The next revision is checked for overflow during resolution.
    pub fn previous(mut self, report: &SettingsReport) -> Self {
        self.previous = Some(report.clone());
        self
    }
}
