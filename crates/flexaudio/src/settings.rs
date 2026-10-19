//! Facade settings preflight, before constructing any native backend.

use crate::{
    resolve_settings, Error, OpenOptions, Result, SettingsReport, SettingsRequest, StreamConfig,
};

pub(crate) fn preflight(
    config: &StreamConfig,
    options: &OpenOptions,
    denoise: Option<bool>,
    previous: Option<&SettingsReport>,
    retain_options: bool,
) -> Result<SettingsReport> {
    let mut request = SettingsRequest::new(config.clone()).open_options(options);
    if retain_options {
        request.keepalive = None;
        request.attachments.denoise = None;
        request.attachments.vad = None;
        request.attachments.secondary_encoding = None;
        request.attachments.legacy_sentinels.clear();
    }
    if let Some(enabled) = denoise {
        request.attachments.denoise = Some(enabled);
    }
    if let Some(previous) = previous {
        request = request.previous(previous);
    }
    let report = resolve_settings(&request)?;
    if options.strict && report.has_ignored() {
        Err(Error::IgnoredSettings { report })
    } else {
        Ok(report)
    }
}
