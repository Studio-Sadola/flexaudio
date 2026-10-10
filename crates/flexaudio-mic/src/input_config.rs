//! Match the actual selected device configuration to the sink before capture builds.

use flexaudio_core::types::{Error, Result};

pub(crate) fn checked<C>(
    discover: impl FnOnce() -> Result<C>,
    native_format: impl FnOnce(&C) -> (u32, u16),
    advertised: (u32, u16),
) -> Result<C> {
    let config = discover()?;
    let actual = native_format(&config);
    if actual.0 == 0 || actual.1 == 0 {
        return Err(Error::InvalidArg(
            "native microphone rate and channels must be positive".into(),
        ));
    }
    if actual.1 > 2 {
        return Err(Error::UnsupportedFormat(
            "native microphone channels must be one or two".into(),
        ));
    }
    if actual != advertised {
        return Err(Error::NativeFormatChanged { advertised, actual });
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_policy::{preflight, PromptCoordinator, Provider, Status};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn invalid_or_multichannel_native_config_is_rejected_before_capture() {
        for native in [(0, 1), (48_000, 0)] {
            assert!(matches!(
                checked(|| Ok(native), |config| *config, native),
                Err(Error::InvalidArg(_))
            ));
        }
        for channels in [3, 6] {
            let native = (48_000, channels);
            assert!(matches!(
                checked(|| Ok(native), |config| *config, native),
                Err(Error::UnsupportedFormat(_))
            ));
        }
    }

    struct PromptingProvider(AtomicBool);

    impl Provider for PromptingProvider {
        fn status(&self) -> Result<Status> {
            Ok(if self.0.load(Ordering::SeqCst) {
                Status::Authorized
            } else {
                Status::NotDetermined
            })
        }

        fn has_usage_description(&self) -> Result<bool> {
            Ok(true)
        }

        fn request_access(&self, completion: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
            self.0.store(true, Ordering::SeqCst);
            completion(true);
            Ok(())
        }
    }

    #[test]
    fn granted_prompt_cannot_feed_real_stereo_config_into_fallback_mono_sink() {
        let provider = PromptingProvider(AtomicBool::new(false));
        preflight(&provider, &PromptCoordinator::default(), Duration::ZERO).unwrap();
        let discover = || {
            assert_eq!(provider.status()?, Status::Authorized);
            Ok((44_100, 2))
        };
        assert_eq!(
            checked(discover, |config| *config, (48_000, 1)),
            Err(Error::NativeFormatChanged {
                advertised: (48_000, 1),
                actual: (44_100, 2),
            })
        );
        // Reconfiguring the sink/normalizer permits precisely that real config.
        assert_eq!(
            checked(discover, |config| *config, (44_100, 2)),
            Ok((44_100, 2))
        );
    }

    #[test]
    fn matching_config_is_preserved_and_query_errors_do_not_invent_a_format() {
        assert_eq!(
            checked(|| Ok((48_000, 2)), |c| *c, (48_000, 2)),
            Ok((48_000, 2))
        );
        assert_eq!(
            checked(
                || Err::<(u32, u16), _>(Error::DeviceNotFound),
                |c| *c,
                (48_000, 1)
            ),
            Err(Error::DeviceNotFound)
        );
    }
}
