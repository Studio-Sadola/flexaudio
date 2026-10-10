//! Pure mapping for Windows microphone capability access.

use flexaudio_core::types::{Error, Permission, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum Access {
    Allowed,
    DeniedByUser,
    DeniedBySystem,
    Unknown,
}

pub(crate) trait Provider {
    fn access(&self) -> Result<Access>;
}

pub(crate) fn preflight(provider: &dyn Provider) -> Result<Access> {
    // Unsupported APIs, ambiguous statuses and failed queries retain Unknown. They
    // must not invent a denial or claim that Windows authorized capture.
    let access = match provider.access() {
        Ok(access) => access,
        Err(_) => Access::Unknown,
    };
    let detail = match access {
        Access::DeniedByUser => Some("Windows microphone access was denied by the user"),
        Access::DeniedBySystem => {
            Some("Windows microphone access was denied by the system privacy policy")
        }
        Access::Allowed | Access::Unknown => None,
    };
    match detail {
        Some(detail) => Err(Error::PermissionDenied {
            permission: Permission::Microphone,
            detail: detail.into(),
        }),
        None => Ok(access),
    }
}

pub(crate) fn after_failure(provider: &dyn Provider, original: Error) -> Error {
    match preflight(provider) {
        Err(denial) => denial,
        Ok(_) => original,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<Access>);
    impl Provider for Fake {
        fn access(&self) -> Result<Access> {
            self.0.clone()
        }
    }

    #[test]
    fn allowed_and_unknown_proceed_with_distinct_statuses() {
        for access in [Access::Allowed, Access::Unknown] {
            assert_eq!(preflight(&Fake(Ok(access))), Ok(access));
        }
    }

    #[test]
    fn authoritative_denials_name_their_cause() {
        for access in [Access::DeniedByUser, Access::DeniedBySystem] {
            assert!(matches!(
                preflight(&Fake(Ok(access))),
                Err(Error::PermissionDenied {
                    permission: Permission::Microphone,
                    ..
                })
            ));
        }
    }

    #[test]
    fn query_failure_proceeds_without_claiming_authorization() {
        assert_eq!(
            preflight(&Fake(Err(Error::Unsupported))),
            Ok(Access::Unknown)
        );
    }

    #[test]
    fn recheck_after_build_or_play_failure_preserves_or_replaces_typed_error() {
        let original = Error::DeviceNotFound;
        for access in [Access::Allowed, Access::Unknown] {
            assert_eq!(after_failure(&Fake(Ok(access)), original.clone()), original);
        }
        assert_eq!(
            after_failure(&Fake(Err(Error::Unsupported)), original.clone()),
            original
        );
        for access in [Access::DeniedByUser, Access::DeniedBySystem] {
            assert!(matches!(
                after_failure(&Fake(Ok(access)), original.clone()),
                Error::PermissionDenied {
                    permission: Permission::Microphone,
                    ..
                }
            ));
        }
    }
}
