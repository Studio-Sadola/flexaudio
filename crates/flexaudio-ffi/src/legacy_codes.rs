//! Stable v0.4 C codes, exported even though ABI fields now use raw i32.
//! Keep these values independent of enum reachability in cbindgen.

pub const FLEX_SOURCE_KIND_MIC: i32 = 0;
pub const FLEX_SOURCE_KIND_SYSTEM: i32 = 1;
pub const FLEX_SOURCE_KIND_PROCESS: i32 = 2;
pub const FLEX_SOURCE_KIND_MIX: i32 = 3;
pub const FLEX_PROCESS_MODE_INCLUDE: i32 = 0;
pub const FLEX_PROCESS_MODE_EXCLUDE: i32 = 1;
pub const FLEX_EVENT_KIND_CHUNK_DROPPED: i32 = 0;
pub const FLEX_EVENT_KIND_STALLED: i32 = 1;
pub const FLEX_EVENT_KIND_RECOVERED: i32 = 2;
pub const FLEX_EVENT_KIND_PERMISSION_DENIED: i32 = 3;
pub const FLEX_EVENT_KIND_DEVICE_LOST: i32 = 4;
pub const FLEX_EVENT_KIND_ERROR: i32 = 5;
pub const FLEX_EVENT_KIND_UNKNOWN: i32 = 6;
pub const FLEX_EVENT_KIND_SILENCE_WHILE_SOURCE_ACTIVE: i32 = 7;
pub const FLEX_EVENT_KIND_PERMISSION_PENDING: i32 = 8;
pub const FLEX_OUTPUT_ACTIVITY_UNKNOWN: i32 = 0;
pub const FLEX_OUTPUT_ACTIVITY_INACTIVE: i32 = 1;
pub const FLEX_OUTPUT_ACTIVITY_ACTIVE: i32 = 2;
pub const FLEX_DEVICE_EVENT_KIND_ADDED: i32 = 0;
pub const FLEX_DEVICE_EVENT_KIND_REMOVED: i32 = 1;
pub const FLEX_DEVICE_EVENT_KIND_DEFAULT_CHANGED: i32 = 2;
pub const FLEX_DEVICE_EVENT_KIND_UNKNOWN: i32 = 3;
