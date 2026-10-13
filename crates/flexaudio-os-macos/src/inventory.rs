//! Native per-entry policy, using the shared collection owner.

use flexaudio_core::Result;

pub(crate) fn collect_complete<I, T>(
    ids: impl IntoIterator<Item = I>,
    mut read: impl FnMut(I) -> Result<Option<T>>,
) -> Result<Vec<T>> {
    flexaudio_core::inventory::collect(Ok(ids), |id| read(id).ok().flatten())
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
                    Err(Error::DeviceNotFound)
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
mod wiring_tests {
    #[test]
    fn native_device_and_process_discovery_use_complete_snapshots() {
        // Native failure injection needs the OS. Keep the portable helper tests
        // connected to both production adapters without accessing audio hardware.
        let devices = include_str!("devices.rs");
        let devices = devices
            .split("pub fn list_output_devices()")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(devices.contains("crate::inventory::collect_complete"));
        assert!(!devices.contains("unwrap_or(FALLBACK_FORMAT)"));
        let processes = include_str!("processes.rs");
        assert!(processes.contains("crate::inventory::collect_complete"));
        assert!(!processes.contains("GetCount().unwrap_or(0)"));
        assert!(!processes.contains("last_error: Option<Error>"));
    }

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
