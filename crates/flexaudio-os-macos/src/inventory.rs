//! Native per-entry policy, using the shared collection owner.

use flexaudio_core::Result;

#[cfg(test)]
mod tests {
    use flexaudio_core::inventory::collect;

    // Native discovery errors cannot be triggered portably without hardware.
    // Inject read results into the helper used by both device and process queries.
    #[test]
    fn unreadable_identity_omits_only_that_entry() {
        assert_eq!(
            collect(Ok([0, 1, 2]), |id| {
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
    fn only_successful_ineligible_entries_are_skipped() {
        assert_eq!(
            collect(Ok([0, 1, 2]), |id| Ok((id != 1).then_some(id))),
            Ok(vec![0, 2])
        );
        assert_eq!(
            collect(Ok(Vec::<u32>::new()), |id| Ok(Some(id))),
            Ok(Vec::new())
        );
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

// Share direction/identity ordering; only the requested device can fail an open.
pub(crate) fn output_device_matches(
    direction: Result<bool>,
    identity_matches: impl FnOnce() -> bool,
) -> Result<bool> {
    match direction {
        Ok(false) => Ok(false),
        Ok(true) => Ok(identity_matches()),
        Err(error)
            if flexaudio_core::inventory::macos_error_failure(&error)
                == flexaudio_core::inventory::ProcessFailure::Vanished =>
        {
            Ok(false)
        }
        Err(error) if identity_matches() => Err(error),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod open_tests {
    use super::*;
    fn direction_error(value: i32) -> flexaudio_core::Error {
        flexaudio_core::Error::Backend("direction query failed; retry open".into()).with_context(
            flexaudio_core::ErrorContext::new(flexaudio_core::Operation::Enumerate)
                .with_native_status(flexaudio_core::NativeStatus::OsStatus {
                    call: "AudioObjectGetPropertyDataSize(OutputStreams)",
                    value,
                }),
        )
    }

    #[test]
    fn unrelated_direction_failure_keeps_healthy_requested_device() {
        let mut selected = None;
        for id in [1, 2] {
            let direction = if id == 1 {
                Err(direction_error(-50))
            } else {
                Ok(true)
            };
            if output_device_matches(direction, || id == 2).unwrap() {
                selected = Some(id);
                break;
            }
        }
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn vanished_direction_failure_skips_identity_query() {
        for value in [0x216f626a, 3, 2] {
            assert_eq!(
                output_device_matches(Err(direction_error(value)), || panic!("vanished identity")),
                Ok(false)
            );
            assert_eq!(output_device_matches(Ok(true), || true), Ok(true));
        }
    }

    #[test]
    fn output_open_retains_direction_query_cause() {
        let error = flexaudio_core::Error::Backend("direction query failed; retry open".into())
            .with_context(
                flexaudio_core::ErrorContext::new(flexaudio_core::Operation::Enumerate)
                    .with_native_status(flexaudio_core::NativeStatus::OsStatus {
                        call: "AudioObjectGetPropertyData(Streams)",
                        value: -50,
                    }),
            );
        assert_eq!(
            output_device_matches(Err(error.clone()), || true),
            Err(error)
        );
        assert_eq!(
            output_device_matches(Ok(false), || panic!("input device identity query")),
            Ok(false)
        );
        assert_eq!(output_device_matches(Ok(true), || true), Ok(true));
    }
}

#[cfg(test)]
mod collection_tests {
    #[test]
    fn native_device_and_process_discovery_use_complete_snapshots() {
        for source in [include_str!("devices.rs"), include_str!("processes.rs")] {
            assert!(source.contains("flexaudio_core::inventory::collect("));
            assert!(!source.contains("collect_complete("));
        }
        let error =
            flexaudio_core::Error::Backend("collection query failed; retry discovery".into());
        assert_eq!(
            flexaudio_core::inventory::collect::<u32, u32>(
                Err::<Vec<u32>, _>(error.clone()),
                |_| panic!("entry query after collection failure")
            ),
            Err(error)
        );
    }
}
