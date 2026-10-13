//! Native per-entry policy, using the shared collection owner.

use flexaudio_core::Result;

pub(crate) fn collect_complete<I, T>(
    ids: impl IntoIterator<Item = I>,
    read: impl FnMut(I) -> Result<Option<T>>,
) -> Result<Vec<T>> {
    flexaudio_core::inventory::collect(Ok(ids), read)
}

pub(crate) fn positive_format(rate: u32, channels: u16) -> Result<(u32, u16)> {
    if rate == 0 || channels == 0 {
        return Err(flexaudio_core::Error::Backend(
            "output device format is invalid; retry device discovery".into(),
        )
        .with_context(flexaudio_core::ErrorContext::new(
            flexaudio_core::Operation::Enumerate,
        )));
    }
    Ok((rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio_core::Error;

    // Native discovery errors cannot be triggered portably without hardware.
    // Inject read results into the helper used by both device and process queries.
    #[test]
    fn unreadable_identity_omits_only_that_entry() {
        assert_eq!(
            collect_complete([0, 1, 2], |id| {
                if id == 1 {
                    Ok(None)
                } else {
                    Ok(Some(id))
                }
            }),
            Ok(vec![0, 2])
        );
    }

    #[test]
    fn all_endpoint_activate_failures_preserve_permission_denied() {
        let error = Error::PermissionDenied {
            permission: flexaudio_core::Permission::SystemAudio,
            detail: "Activate access denied; retry".into(),
        };
        assert_eq!(
            collect_complete([0, 1, 2], |_| Err::<Option<u32>, _>(error.clone())),
            Err(error)
        );
    }

    #[test]
    fn one_endpoint_session_count_failure_rejects_the_collection() {
        let error = Error::Backend("GetCount failed; retry".into());
        assert_eq!(
            collect_complete([0, 1, 2], |id| {
                if id == 1 {
                    Err(error.clone())
                } else {
                    Ok(Some(vec![id]))
                }
            }),
            Err(error)
        );
    }

    #[test]
    fn vanished_session_keeps_neighboring_sessions_and_endpoints() {
        let result = collect_complete([0, 1], |endpoint| {
            collect_complete([0, 1, 2], |session| {
                Ok((endpoint != 0 || session != 1).then_some((endpoint, session)))
            })
            .map(Some)
        })
        .unwrap();
        assert_eq!(result, [vec![(0, 0), (0, 2)], vec![(1, 0), (1, 1), (1, 2)]]);
    }

    #[test]
    fn only_successful_ineligible_entries_are_skipped() {
        assert_eq!(
            collect_complete([0, 1, 2], |id| Ok((id != 1).then_some(id))),
            Ok(vec![0, 2])
        );
        assert_eq!(
            collect_complete(Vec::<u32>::new(), |id| Ok(Some(id))),
            Ok(Vec::new())
        );
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    #[test]
    fn mandatory_native_format_never_substitutes_a_default() {
        assert!(positive_format(0, 2).is_err());
        assert!(positive_format(48_000, 0).is_err());
        assert_eq!(positive_format(44_100, 6), Ok((44_100, 6)));
    }
}

#[cfg(test)]
mod wiring_tests {
    #[test]
    fn native_capture_owners_project_spawn_errors_through_core() {
        // OS thread-creation exhaustion is not a portable fault. The core helper
        // tests retain io kind/code; this guards its wiring into both native owners.
        for source in [include_str!("system.rs"), include_str!("process.rs")] {
            let spawn = source
                .split(".spawn(move ||")
                .nth(1)
                .unwrap()
                .split("})?;")
                .next()
                .unwrap();
            assert!(spawn.contains("flexaudio_core::spawn_error"));
        }
    }
}
