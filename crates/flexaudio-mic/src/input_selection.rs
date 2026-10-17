//! Retain one device/configuration query from discovery until capture starts.

use flexaudio_core::types::Result;

pub(crate) struct InputSelection<D, C> {
    pending: Option<(D, C)>,
}

impl<D, C> InputSelection<D, C> {
    pub(crate) fn empty() -> Self {
        Self { pending: None }
    }

    pub(crate) fn query(
        query: impl FnOnce() -> Result<(D, C)>,
        format: impl FnOnce(&C) -> (u32, u16),
    ) -> Result<(Self, (u32, u16))> {
        let selected = query()?;
        let native = format(&selected.1);
        Ok((
            Self {
                pending: Some(selected),
            },
            native,
        ))
    }

    pub(crate) fn take_or_query(
        &mut self,
        query: impl FnOnce() -> Result<(D, C)>,
    ) -> Result<(D, C)> {
        match self.pending.take() {
            Some(selected) => Ok(selected),
            None => query(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::types::Error;
    use std::cell::Cell;

    #[test]
    fn one_resolution_and_format_query_per_open() {
        for device_id in [None, Some("selected microphone")] {
            for _ in 0..2 {
                let resolutions = Cell::new(0);
                let configs = Cell::new(0);
                let query = || {
                    resolutions.set(resolutions.get() + 1);
                    let device = device_id.unwrap_or("default microphone");
                    configs.set(configs.get() + 1);
                    Ok((device, (44_100, 2)))
                };
                let (mut selection, native) =
                    InputSelection::query(query, |config| *config).unwrap();
                assert_eq!(native, (44_100, 2));
                let selected = selection.take_or_query(query).unwrap();
                assert_eq!(
                    selected,
                    (device_id.unwrap_or("default microphone"), (44_100, 2))
                );
                assert_eq!(
                    resolutions.get(),
                    1,
                    "one resolution per open: {device_id:?}"
                );
                assert_eq!(configs.get(), 1, "one format query per open: {device_id:?}");

                // A restart must discover the current device/format, not reuse a cache.
                let restarted = selection
                    .take_or_query(|| {
                        resolutions.set(resolutions.get() + 1);
                        configs.set(configs.get() + 1);
                        Ok(("reconnected microphone", (48_000, 1)))
                    })
                    .unwrap();
                assert_eq!(restarted, ("reconnected microphone", (48_000, 1)));
                assert_eq!(resolutions.get(), 2);
                assert_eq!(configs.get(), 2);
            }
        }
    }

    #[test]
    fn missing_device_fails_during_discovery() {
        let result =
            InputSelection::<(), ()>::query(|| Err(Error::DeviceNotFound), |_| (48_000, 1));
        assert!(matches!(result, Err(Error::DeviceNotFound)));
    }
}
