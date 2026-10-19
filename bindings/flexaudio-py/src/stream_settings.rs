//! Transactional settings preflight shared by source switches and device-free verification.
use super::*;

impl Stream {
    pub(super) fn capture_settings(&self) -> Option<&fa::SettingsReport> {
        let report = self.inner.effective_settings();
        #[cfg(test)]
        let report = report.or(self.settings.report.as_ref());
        report
    }

    pub(super) fn switch_with(
        &mut self,
        config: fa::StreamConfig,
        denoise: bool,
        keepalive: Option<bool>,
        strict: Option<bool>,
        commit: impl FnOnce(&mut fa::Stream, fa::StreamConfig) -> fa::Result<()>,
    ) -> PyResult<()> {
        if self.shutdown.is_some() {
            return Err(to_py_err(fa::Error::InvalidState(
                "switch_source is only available on a started stream".into(),
            )));
        }
        validate_denoise(denoise, self.inner.config().output.sample_rate)?;
        if config.output != self.inner.config().output {
            return Err(to_py_err(fa::Error::InvalidArg(
                "output format cannot change during switch_source".into(),
            )));
        }
        crate::settings_validation::validate(&config, false).map_err(to_py_err)?;
        let strict = strict.unwrap_or(self.settings.strict);
        let mut previous = self.capture_settings().cloned();
        if let Some(entry) = previous.as_mut().and_then(|report| {
            report
                .entries
                .iter_mut()
                .find(|entry| entry.setting == fa::SettingKey::Gain)
        }) {
            entry.effective = fa::SettingValue::Float(self.inner.gain());
        }
        let report = crate::settings::Request {
            config: &config,
            keepalive,
            denoise,
            vad: self.vad_tap.is_some(),
            secondary: self.secondary.as_ref(),
        }
        .resolve(previous.as_ref(), strict)
        .map_err(to_py_err)?;
        commit(&mut self.inner, config).map_err(to_py_err)?;
        self.settings = crate::settings::SettingsState {
            #[cfg(test)]
            report: self
                .inner
                .effective_settings()
                .is_none()
                .then_some(report.clone()),
            strict,
        };
        let _ = report;
        Ok(())
    }
}
