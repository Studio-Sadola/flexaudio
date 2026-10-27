//! Internal binding contract fixtures; no stability guarantee.

pub use crate::error_context::shutdown_fields;
pub use crate::errors::error_cases;

/// Inject two unequal failures at one recording point and verify ordered retention.
/// The fixture supplies the real recording operation and its reconciled outcome.
pub fn assert_distinct_failures_retained(
    first: crate::Error,
    second: crate::Error,
    exercise: impl FnOnce(crate::Error, crate::Error) -> crate::Error,
) {
    assert_ne!(
        first, second,
        "the fixture must inject different failure values"
    );
    let retained = exercise(first.clone(), second.clone());
    assert!(
        crate::terminal_causes::retains_terminal_cause(&retained, &first),
        "first failure lost: {retained:?}"
    );
    assert!(
        crate::terminal_causes::retains_terminal_cause(&retained, &second),
        "later distinct failure lost: {retained:?}"
    );
    match retained {
        crate::Error::Multiple(group) => {
            assert_eq!(group.primary(), &first, "observation order changed");
            assert_eq!(group.secondary().next(), Some(&second));
        }
        error => panic!("distinct failures were not grouped: {error:?}"),
    }
}

#[cfg(test)]
mod retention_fixture_tests {
    use super::*;

    #[test]
    fn distinct_failure_fixture_rejects_a_dropped_later_failure() {
        let result = std::panic::catch_unwind(|| {
            assert_distinct_failures_retained(
                crate::Error::Backend("first fixture failure".into()),
                crate::Error::Backend("second fixture failure".into()),
                |first, _second| first,
            );
        });
        assert!(
            result.is_err(),
            "the shared fixture must detect lost failures"
        );
    }
}

// Each list also generates an exhaustive match in the defining crate, so a new
// typed cause cannot silently miss the shared binding fixtures.
macro_rules! variant_inventory {
    ($vis:vis $name:ident, $ty:ident, [$($variant:ident),+ $(,)?]) => {
        /// Every variant, checked exhaustively inside its defining crate.
        /// Internal test support only; no stability guarantee.
        $vis fn $name() -> Vec<crate::$ty> {
            fn covered(value: crate::$ty) {
                match value { $(crate::$ty::$variant => ()),+ }
            }
            let values = vec![$(crate::$ty::$variant),+];
            for value in &values {
                covered(*value);
            }
            values
        }
    };
}

variant_inventory!(
    pub operations,
    Operation,
    [
        Enumerate, Start, Normalize, Flush, Reopen, Rollback, Stop, Join, Link,
    ]
);
variant_inventory!(
    pub helper_stop_reasons,
    HelperStopReason,
    [
        SignalObserved,
        PermissionDenied,
        Inconclusive,
        TimedOut,
        StreamStopped,
        SourceSwitched,
        Recovery,
        StartupFailed,
        RuntimeFailed,
        Cancelled,
    ]
);

variant_inventory!(
    addon_kinds,
    AddonErrorKind,
    [
        VadModelLoad,
        VadInference,
        VadInvalidConfig,
        VadInvalidFormat,
        VadResample,
        VadReset,
        FlacIo,
        FlacUnsupported,
        FlacEncoder,
    ]
);
variant_inventory!(
    vad_codes,
    VadErrorCode,
    [
        InvalidParameter,
        InvalidProbability,
        InvalidPcm,
        Overflow,
        SessionFinished,
        Inference,
        ModelLoad,
        FailedSession,
        InvalidStereoLength,
        CaptureSampleOverflow,
        PtsOutOfRange,
        UnsupportedTap,
        UnsupportedConversionClock,
        Conversion,
        Stopped,
    ]
);

macro_rules! error_shapes {
    ($($name:literal => $pattern:pat),+ $(,)?) => {
        fn error_shape(error: &crate::Error) -> &'static str {
            match error { $($pattern => $name),+ }
        }
        const ERROR_SHAPES: &[&str] = &[$($name),+];
    };
}

error_shapes! {
    "InvalidArg" => crate::Error::InvalidArg(_),
    "InvalidState" => crate::Error::InvalidState(_),
    "DeviceNotFound" => crate::Error::DeviceNotFound,
    "PermissionDenied" => crate::Error::PermissionDenied { .. },
    "UnsupportedOsVersion" => crate::Error::UnsupportedOsVersion,
    "DeviceLost" => crate::Error::DeviceLost,
    "Backend" => crate::Error::Backend(_),
    "Addon" => crate::Error::Addon { .. },
    "Vad" => crate::Error::Vad { .. },
    "UnsupportedFormat" => crate::Error::UnsupportedFormat(_),
    "NativeFormatChanged" => crate::Error::NativeFormatChanged { .. },
    "Unsupported" => crate::Error::Unsupported,
    "AmbiguousDeviceName" => crate::Error::AmbiguousDeviceName,
    "Diagnostic" => crate::Error::Diagnostic { .. },
    "Context" => crate::Error::Context { .. },
    "Multiple" => crate::Error::Multiple(_),
    "IgnoredSettings" => crate::Error::IgnoredSettings { .. },
}

/// Representative values of every error variant, addon kind and VAD code,
/// both directly and inside context, diagnostic and related-error wrappers.
/// Internal test support only; no stability guarantee.
pub fn error_variant_cases() -> Vec<(crate::ErrorKind, &'static str, crate::Error)> {
    use crate::{Error, ErrorContext, ErrorGroup, Operation};

    let canonical = error_cases();
    let mut roots: Vec<_> = canonical
        .iter()
        .map(|(_, _, error)| error.clone())
        .collect();
    roots.extend(addon_kinds().into_iter().map(|kind| Error::Addon {
        kind,
        message: "addon diagnostic".into(),
    }));
    roots.extend(vad_codes().into_iter().map(|code| Error::Vad { code }));
    let mut cases = Vec::new();
    for error in roots {
        let kind = error.kind();
        let name = canonical
            .iter()
            .find(|(candidate, _, _)| *candidate == kind)
            .expect("every variant kind must have a canonical core name")
            .1;
        let wrapped = [
            error.clone(),
            error
                .clone()
                .with_context(ErrorContext::new(Operation::Stop)),
            error
                .clone()
                .with_diagnostic_detail("fixture diagnostic".into()),
            Error::Multiple(ErrorGroup::new(error, Error::DeviceLost, Vec::new())),
        ];
        for error in wrapped {
            assert_eq!(
                error.kind(),
                kind,
                "wrappers must preserve the primary kind"
            );
            cases.push((kind, name, error));
        }
    }
    for shape in ERROR_SHAPES {
        assert!(
            cases
                .iter()
                .any(|(_, _, error)| error_shape(error) == *shape),
            "core error variant has no binding fixture: {shape}"
        );
    }
    cases
}

/// C and Node code symbol for a canonical camelCase kind name.
///
/// `backend` is the one existing ABI exception and is spelled `FLEX_FAILURE`.
/// The symbol is derived from the name; this function does not list kinds.
pub fn canonical_error_code_symbol(name: &str) -> String {
    if name == "backend" {
        return "FLEX_FAILURE".to_owned();
    }
    let mut symbol = String::from("FLEX_");
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            symbol.push('_');
            symbol.push(ch);
        } else {
            symbol.push(ch.to_ascii_uppercase());
        }
    }
    symbol
}

/// Extract declaration field names, ignoring whole-line and block documentation.
/// This intentionally supports the simple TypeScript interface and Python TypedDict
/// declarations used by the bindings, rather than parsing either language generally.
pub fn declaration_fields(source: &str, start: &str, end: &str) -> Vec<String> {
    let body = source.split_once(start).expect("contract declaration").1;
    let body = body.split_once(end).expect("contract declaration end").0;
    let mut comment = false;
    let mut fields = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("/*") {
            comment = true;
        }
        if comment {
            if line.contains("*/") {
                comment = false;
            }
            continue;
        }
        if line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once(':') {
            fields.push(name.trim().to_owned());
        }
    }
    fields.sort();
    fields
}
