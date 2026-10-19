//! Facade settings preflight, before constructing any native backend.

use crate::settings_resolver::{resolve_settings, SettingsPlatform, SettingsRequest};
use crate::{Error, OpenOptions, Result, SettingsReport, StreamConfig};

pub(crate) fn preflight(
    config: &StreamConfig,
    options: &OpenOptions,
    denoise: Option<bool>,
    previous: Option<&SettingsReport>,
    retain_options: bool,
) -> Result<SettingsReport> {
    let mut request = SettingsRequest::new(config.clone());
    request.windows_classic_loopback_keepalive = options.windows_classic_loopback_keepalive;
    if retain_options {
        request.retained_windows_classic_loopback_keepalive =
            Some(options.windows_classic_loopback_keepalive);
    }
    request.denoise = denoise.unwrap_or(false);
    if denoise.is_none() {
        request.retained_denoise = previous
            .and_then(|report| report.get(crate::SettingKey::Denoise))
            .and_then(|entry| match entry.effective {
                crate::SettingValue::Bool(value) => Some(value),
                _ => None,
            });
    }
    let report = resolve_settings(
        &request,
        SettingsPlatform::current(),
        std::process::id(),
        previous,
    )?;
    if options.strict && report.has_ignored() {
        Err(Error::IgnoredSettings { report })
    } else {
        Ok(report)
    }
}
