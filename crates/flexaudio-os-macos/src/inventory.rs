//! Native per-entry policy, using the shared collection owner.

use flexaudio_core::Result;

pub(crate) fn collect_complete<I, T>(
    ids: impl IntoIterator<Item = I>,
    read: impl FnMut(I) -> Result<Option<T>>,
) -> Result<Vec<T>> {
    flexaudio_core::inventory::collect(Ok(ids), read)
}

#[cfg(test)]
mod tests {
    use super::*;

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
