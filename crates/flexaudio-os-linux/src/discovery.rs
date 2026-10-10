// Failure steps for authoritative device inventory queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum EnumerationFailure {
    SecondSync,
    CoreGone,
    CoreError,
    Iterate,
    Deadline,
    Identity,
    Format,
    Metadata,
    Callback,
}
impl EnumerationFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::SecondSync => "pipewire second registry sync failed",
            Self::CoreGone => "pipewire core disappeared during registry enumeration",
            Self::CoreError => "pipewire core rejected the registry query",
            Self::Iterate => "pipewire registry loop iteration failed",
            Self::Deadline => "pipewire registry enumeration timed out",
            Self::Identity => "pipewire device has no mandatory stable identity",
            Self::Format => "pipewire device advertised an invalid native format",
            Self::Metadata => "pipewire default metadata query failed",
            Self::Callback => "pipewire registry query callback rejected",
        }
    }
}
