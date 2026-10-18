//! Pure WASAPI process-tree exclusion policy shared by capture and capabilities.

use flexaudio_core::{Error, Result};

pub(crate) const MAX_DISTINCT_EXCLUSION_PIDS: usize = 1;

/// Validate WASAPI's single-tree exclusion without COM or process-tree guesses.
pub(crate) fn validate_exclusion(
    exclude_self: bool,
    self_pid: u32,
    pids: &[u32],
) -> Result<Option<u32>> {
    let root = if exclude_self {
        Some(self_pid)
    } else {
        pids.first().copied()
    };
    let Some(root) = root else {
        return Ok(None);
    };

    let mut distinct_pids = std::collections::BTreeSet::from([root]);
    for &pid in pids {
        if pid == 0 {
            return Err(Error::InvalidArg(
                "exclude_pids: pid 0 is not a valid process id".into(),
            ));
        }
        distinct_pids.insert(pid);
        if distinct_pids.len() > MAX_DISTINCT_EXCLUSION_PIDS {
            return Err(Error::InvalidArg(format!(
                "exclude_pids: pid {pid} is outside the single process tree WASAPI can exclude (root {root}); Windows supports excluding one process tree per capture"
            )));
        }
    }
    Ok(Some(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_exclusion_accepts_root_only() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42]),
            Ok(Some(42))
        ));
    }

    #[test]
    fn validate_exclusion_accepts_duplicate_root() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42, 42]),
            Ok(Some(42))
        ));
    }

    #[test]
    fn validate_exclusion_accepts_self_pid() {
        assert!(matches!(
            validate_exclusion(true, 100, &[100]),
            Ok(Some(100))
        ));
        assert!(matches!(validate_exclusion(true, 100, &[]), Ok(Some(100))));
    }

    #[test]
    fn validate_exclusion_rejects_foreign_pid_with_exclude_self() {
        assert!(matches!(
            validate_exclusion(true, 100, &[100, 42]),
            Err(Error::InvalidArg(message))
                if message.starts_with("exclude_pids: pid 42 ")
                    && message.contains("root 100")
        ));
    }

    #[test]
    fn validate_exclusion_rejects_second_tree() {
        assert!(matches!(
            validate_exclusion(false, 100, &[42, 99]),
            Err(Error::InvalidArg(message))
                if message.starts_with("exclude_pids: pid 99 ")
                    && message.contains("root 42")
        ));
    }

    #[test]
    fn validate_exclusion_rejects_pid_zero() {
        for (exclude_self, pids) in [(false, vec![0]), (false, vec![42, 0]), (true, vec![0])] {
            assert!(matches!(
                validate_exclusion(exclude_self, 100, &pids),
                Err(Error::InvalidArg(message)) if message.starts_with("exclude_pids: pid 0 ")
            ));
        }
    }

    #[test]
    fn validate_exclusion_empty_list_has_no_root() {
        assert!(matches!(validate_exclusion(false, 100, &[]), Ok(None)));
    }
}
