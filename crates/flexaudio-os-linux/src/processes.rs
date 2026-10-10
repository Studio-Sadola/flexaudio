//! Enumerate capturable processes from the PipeWire registry.
//!
//! A short-lived main loop collects Client PIDs, names and `client.api`, plus
//! output Node ownership, names and protocol provenance. Bound node info
//! supplies application PIDs and Running/Idle state.
//!
//! PID resolution shares [`resolve_node_pid`](crate::resolve_node_pid) with
//! process capture. Pulse nodes require a valid app PID from bound node info;
//! unresolved output nodes fail the query instead of being listed under the proxy's PID.
//!
//! Get executable names from the kernel's `/proc/<pid>/exe`, not PipeWire's self-reported value
//! (fall back to `/proc/<pid>/comm` if unreadable).
//!
//! # Deadline
//! Use the same [`LIST_DEADLINE`] for the connection (`connect`) and registry round trip. Always
//! return, even if there is no response. Incomplete snapshots always return an error, including
//! when output nodes have already arrived. A completed query with no output nodes returns `Ok([])`.
//!
//! Return a raw list (multiple nodes with the same PID may appear more than once). The facade merges
//! duplicates, excludes the current process, and sorts the results.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::Duration;

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{Error, ProcessInfo, Result};

use pipewire as pw;

use crate::{
    is_pulse_proxied, pid_from_props, pw_init_once, resolve_node_pid, update_node_info,
    ClientEntry, NodeEntry,
};

/// Deadline for connection + registry round trip; incomplete queries return `Err`.
const LIST_DEADLINE: Duration = Duration::from_millis(2_000);

/// `media.class` for application output nodes targeted by process capture.
const OUTPUT_STREAM_CLASS: &str = "Stream/Output/Audio";

/// One application output node collected from the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputNode {
    /// Used to resolve the PID (same format as process capture).
    entry: NodeEntry,
    /// Node's `application.name` (self-reported by the app, for display).
    app_name: Option<String>,
    /// Whether the node is Running (`Some` only after info arrives for a bound node).
    running: Option<bool>,
}

/// Results from one registry round trip (PipeWire-independent and constructible in tests).
#[derive(Debug, Default)]
struct RegistrySnapshot {
    /// Client global id -> PID and protocol provenance.
    client_pid: HashMap<u32, ClientEntry>,
    /// Client global ID → that Client's `application.name`.
    client_name: HashMap<u32, String>,
    /// Node global ID → application output node.
    nodes: HashMap<u32, OutputNode>,
}

/// Typed internal failures, rendered at the existing public `Error::Backend` boundary.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
enum SnapshotError {
    Query {
        operation: &'static str,
        cause: String,
    },
    Bind {
        node_id: u32,
        cause: String,
    },
    Callback {
        callback: &'static str,
        cause: String,
    },
    Core {
        object_id: u32,
        code: i32,
        cause: String,
    },
    Incomplete,
    MissingPid {
        node_id: u32,
    },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query { operation, cause } => write!(f, "pipewire {operation} failed: {cause}"),
            Self::Bind { node_id, cause } => {
                write!(f, "pipewire bind output node {node_id} failed: {cause}")
            }
            Self::Callback { callback, .. } => {
                write!(f, "pipewire {callback} callback rejected")
            }
            Self::Core {
                object_id, code, ..
            } => write!(
                f,
                "pipewire object {object_id} error {code}: {}",
                std::io::Error::from_raw_os_error(code.saturating_neg())
            ),
            Self::Incomplete => write!(
                f,
                "pipewire registry sync did not complete within {} ms",
                LIST_DEADLINE.as_millis()
            ),
            Self::MissingPid { node_id } => write!(
                f,
                "pipewire output node {node_id} has no resolvable mandatory process ID"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Preserve the first cause even if later events or the deadline also fail.
fn record_failure(failure: &RefCell<Option<SnapshotError>>, error: SnapshotError) {
    failure.borrow_mut().get_or_insert(error);
}

/// Guard the FFI boundary without silently discarding a failed callback.
fn run_callback(
    failure: &RefCell<Option<SnapshotError>>,
    callback: &'static str,
    body: impl FnOnce(),
) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        record_failure(
            failure,
            SnapshotError::Callback {
                callback,
                cause: "callback rejected".into(),
            },
        );
    }
}

/// List processes with an audio output stream (`Stream/Output/Audio`) as a raw list.
///
/// Returns [`Error::Backend`] for connection, sync, bind or callback failures and unresolved
/// mandatory output-node PIDs. Only a completed empty query returns `Ok([])`. Executable names
/// and activity remain optional metadata.
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    let snapshot = collect_snapshot().map_err(|error| {
        Error::Backend(error.to_string()).with_context(flexaudio_core::ErrorContext::new(
            flexaudio_core::Operation::Enumerate,
        ))
    })?;
    build_process_list(&snapshot, read_executable).map_err(|error| {
        Error::Backend(error.to_string()).with_context(flexaudio_core::ErrorContext::new(
            flexaudio_core::Operation::Enumerate,
        ))
    })
}

/// Convert collection results to a raw list, rejecting unresolved mandatory PIDs.
///
/// Display name preference: node `application.name`, then Client `application.name` (empty if neither
/// exists; the facade fills in an executable name, etc.). Sort deterministically by node ID.
fn build_process_list(
    snapshot: &RegistrySnapshot,
    executable_of: impl Fn(u32) -> Option<String>,
) -> std::result::Result<Vec<ProcessInfo>, SnapshotError> {
    let mut node_ids: Vec<u32> = snapshot.nodes.keys().copied().collect();
    node_ids.sort_unstable();

    let mut out = Vec::with_capacity(node_ids.len());
    for node_id in node_ids {
        let node = &snapshot.nodes[&node_id];
        let pid = resolve_node_pid(&node.entry, &snapshot.client_pid)
            .ok_or(SnapshotError::MissingPid { node_id })?;
        let client_name = node
            .entry
            .owning_client_id
            .and_then(|client_id| snapshot.client_name.get(&client_id).cloned());
        out.push(ProcessInfo {
            pid,
            name: node.app_name.clone().or(client_name).unwrap_or_default(),
            executable: executable_of(pid),
            bundle_id: None,
            is_output_active: node.running,
        });
    }
    Ok(out)
}

/// Basename of `/proc/<pid>/exe` (strip ` (deleted)` for a replaced binary). If unreadable (for
/// example, another user's process), try `/proc/<pid>/comm`. Return `None` if both fail.
fn read_executable(pid: u32) -> Option<String> {
    let from_exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| path.to_str().and_then(clean_exe_path));
    from_exe.or_else(|| {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|comm| comm.trim().to_string())
            .filter(|comm| !comm.is_empty())
    })
}

/// Get the basename of the `/proc/<pid>/exe` symlink target.
fn clean_exe_path(path: &str) -> Option<String> {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    executable_basename(path)
}

/// Convert a props value to `String` only if it is non-empty.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Read the PipeWire registry for one round trip. Returns a typed error on failure (does not panic).
///
/// Create, use, and drop the `!Send` `MainLoop`/`Context`/`Core`/`Registry`/`Node` proxies only
/// inside this function. The facade calls it from a dedicated thread.
///
/// Wait for completion with the same two-phase sync→done barrier as `enumerate_pw` (phase 1 collects
/// all globals; phase 2 waits for info/state from bound nodes). A deadline timer also guarantees the
/// loop exits.
fn collect_snapshot() -> std::result::Result<RegistrySnapshot, SnapshotError> {
    pw_init_once();
    let started = std::time::Instant::now();

    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(|e| SnapshotError::Query {
        operation: "create main loop",
        cause: e.to_string(),
    })?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).map_err(|e| SnapshotError::Query {
            operation: "create context",
            cause: e.to_string(),
        })?;
    // The connection is also inside the deadline. connect itself cannot be interrupted, so if it
    // returns after the deadline, stop without waiting for the registry. If it hangs, the facade's
    // 3-second limit (single-flight) releases the caller.
    let core = context.connect_rc(None).map_err(|e| SnapshotError::Query {
        operation: "connect to daemon",
        cause: e.to_string(),
    })?;
    if started.elapsed() >= LIST_DEADLINE {
        return Err(SnapshotError::Incomplete);
    }
    let registry = core.get_registry_rc().map_err(|e| SnapshotError::Query {
        operation: "get registry",
        cause: e.to_string(),
    })?;

    let snapshot = Rc::new(RefCell::new(RegistrySnapshot::default()));
    let failure = Rc::new(RefCell::new(None));
    // Storage for bound node proxies and listeners (dropping it ends info subscriptions).
    type BoundNode = (pw::node::Node, pw::node::NodeListener);
    let bound_nodes: Rc<RefCell<Vec<BoundNode>>> = Rc::new(RefCell::new(Vec::new()));

    let failure_for_global = failure.clone();
    let snapshot_for_global = snapshot.clone();
    let registry_for_global = registry.clone();
    let bound_for_global = bound_nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            run_callback(&failure_for_global, "registry global", || {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Client => {
                        let client = ClientEntry::from_props(
                            props.get(*pw::keys::APP_PROCESS_ID),
                            props.get(*pw::keys::SEC_PID),
                            props.get(*pw::keys::CLIENT_API),
                        );
                        let mut snap = snapshot_for_global.borrow_mut();
                        snap.client_pid.insert(global.id, client);
                        if let Some(name) = non_empty(props.get(*pw::keys::APP_NAME)) {
                            snap.client_name.insert(global.id, name);
                        }
                    }
                    pw::types::ObjectType::Node => {
                        if props.get(*pw::keys::MEDIA_CLASS) != Some(OUTPUT_STREAM_CLASS) {
                            return;
                        }
                        let entry = NodeEntry {
                            owning_client_id: props
                                .get(*pw::keys::CLIENT_ID)
                                .and_then(|s| s.parse::<u32>().ok()),
                            app_pid: pid_from_props(props.get(*pw::keys::APP_PROCESS_ID), None),
                            app_pid_from_info: false,
                            pulse_proxied: is_pulse_proxied(props.get(*pw::keys::CLIENT_API)),
                            info_seen: false,
                            props_seen: false,
                            // The declared port count is only used by capture's link planner.
                            n_output_ports: None,
                        };
                        snapshot_for_global.borrow_mut().nodes.insert(
                            global.id,
                            OutputNode {
                                entry,
                                app_name: non_empty(props.get(*pw::keys::APP_NAME)),
                                running: None,
                            },
                        );

                        // Bind for state and mandatory application PID information.
                        let node: pw::node::Node = match registry_for_global.bind(global) {
                            Ok(node) => node,
                            Err(error) => {
                                record_failure(
                                    &failure_for_global,
                                    SnapshotError::Bind {
                                        node_id: global.id,
                                        cause: error.to_string(),
                                    },
                                );
                                return;
                            }
                        };
                        let failure_for_info = failure_for_global.clone();
                        let snapshot_for_info = snapshot_for_global.clone();
                        let node_id = global.id;
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                // Catch unwinding at the FFI boundary; state() may
                                // panic on an error string containing invalid UTF-8.
                                run_callback(&failure_for_info, "node info", || {
                                    let running =
                                        matches!(info.state(), pw::node::NodeState::Running);
                                    let props_changed = info
                                        .change_mask()
                                        .contains(pw::node::NodeChangeMask::PROPS);
                                    let props = info.props().map(|p| {
                                        (
                                            p.get(*pw::keys::APP_PROCESS_ID),
                                            p.get(*pw::keys::CLIENT_API),
                                        )
                                    });
                                    let mut snap = snapshot_for_info.borrow_mut();
                                    let RegistrySnapshot {
                                        nodes, client_pid, ..
                                    } = &mut *snap;
                                    if let Some(entry) = nodes.get_mut(&node_id) {
                                        entry.running = Some(running);
                                        update_node_info(
                                            &mut entry.entry,
                                            props_changed,
                                            props,
                                            client_pid,
                                        );
                                    }
                                });
                            })
                            .register();
                        bound_for_global.borrow_mut().push((node, listener));
                    }
                    _ => {}
                }
            });
        })
        .register();

    // Two-phase sync→done barrier (same as enumerate_pw).
    let done = Rc::new(Cell::new(false));
    let stage = Rc::new(Cell::new(0u8));
    let pending = core.sync(0).map_err(|e| SnapshotError::Query {
        operation: "initial sync",
        cause: e.to_string(),
    })?;
    let pending = Rc::new(Cell::new(pending.seq()));

    let failure_for_done = failure.clone();
    let failure_for_core = failure.clone();
    let loop_for_error = main_loop.clone();
    let done_for_cb = done.clone();
    let stage_for_cb = stage.clone();
    let pending_for_cb = pending.clone();
    let loop_for_cb = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            run_callback(&failure_for_done, "core done", || {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                let seq = seq.seq();
                match stage_for_cb.get() {
                    0 if seq == pending_for_cb.get() => {
                        // Phase 1 complete (all globals collected) → phase 2 waits for bound node info.
                        stage_for_cb.set(1);
                        let second = core_weak.upgrade().map(|core| core.sync(0));
                        match second {
                            Some(Ok(p)) => pending_for_cb.set(p.seq()),
                            failed => {
                                let cause = match failed {
                                    Some(Err(error)) => error.to_string(),
                                    None => "core disconnected before second sync".into(),
                                    Some(Ok(_)) => unreachable!(),
                                };
                                record_failure(
                                    &failure_for_done,
                                    SnapshotError::Query {
                                        operation: "second sync",
                                        cause,
                                    },
                                );
                                loop_for_cb.quit();
                            }
                        }
                    }
                    1 if seq == pending_for_cb.get() => {
                        done_for_cb.set(true);
                        loop_for_cb.quit();
                    }
                    _ => {}
                }
            });
        })
        .error(move |object_id, _seq, code, cause| {
            record_failure(
                &failure_for_core,
                SnapshotError::Core {
                    object_id,
                    code,
                    cause: cause.to_string(),
                },
            );
            loop_for_error.quit();
        })
        .register();

    // Deadline timer for the time remaining after connection. Ensures the loop exits even if the
    // registry does not respond. Keep the timer alive throughout run().
    let remaining = LIST_DEADLINE.saturating_sub(started.elapsed());
    let timed_out = Rc::new(Cell::new(remaining.is_zero()));
    let _deadline = if remaining.is_zero() {
        None
    } else {
        let timed_out_for_timer = timed_out.clone();
        let loop_for_timer = main_loop.clone();
        let deadline = main_loop.loop_().add_timer(move |_expirations| {
            timed_out_for_timer.set(true);
            loop_for_timer.quit();
        });
        deadline
            .update_timer(Some(remaining), None)
            .into_result()
            .map_err(|e| SnapshotError::Query {
                operation: "arm deadline timer",
                cause: e.to_string(),
            })?;
        Some(deadline)
    };

    while !done.get() && !timed_out.get() && failure.borrow().is_none() {
        main_loop.run();
    }

    finish_snapshot(
        done.get() && !timed_out.get(),
        failure.take(),
        snapshot.take(),
    )
}

/// Pure completion gate shared by live collection and synthetic event tests.
fn finish_snapshot(
    complete: bool,
    failure: Option<SnapshotError>,
    collected: RegistrySnapshot,
) -> std::result::Result<RegistrySnapshot, SnapshotError> {
    if let Some(error) = failure {
        return Err(error);
    }
    if !complete {
        return Err(SnapshotError::Incomplete);
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(client: Option<u32>, app_pid: Option<u32>, name: Option<&str>) -> OutputNode {
        OutputNode {
            entry: NodeEntry {
                owning_client_id: client,
                app_pid,
                info_seen: false,
                n_output_ports: None,
                ..NodeEntry::default()
            },
            app_name: name.map(str::to_string),
            running: None,
        }
    }

    #[test]
    fn build_resolves_pid_through_client_like_the_capture_backend() {
        let mut snap = RegistrySnapshot::default();
        snap.client_pid
            .insert(40, ClientEntry::from_props(None, Some("1234"), None));
        snap.client_name.insert(40, "Firefox".into());
        snap.client_pid
            .insert(41, ClientEntry::from_props(None, Some("5678"), None));
        // Node for client 40 (has a node name; Running).
        snap.nodes.insert(
            100,
            OutputNode {
                running: Some(true),
                ..node(Some(40), None, Some("Firefox Audio"))
            },
        );
        // Node for client 41 (no name on the node or Client).
        snap.nodes.insert(101, node(Some(41), None, None));
        // Configuration where the node itself has a PID (no Client needed).
        snap.nodes
            .insert(102, node(None, Some(999), Some("direct")));

        let list = build_process_list(&snap, |pid| Some(format!("exe-{pid}"))).unwrap();
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1234, 5678, 999], "sorted by node id");

        let firefox = &list[0];
        assert_eq!(firefox.name, "Firefox Audio", "node application.name wins");
        assert_eq!(firefox.executable.as_deref(), Some("exe-1234"));
        assert_eq!(firefox.is_output_active, Some(true));
        assert_eq!(firefox.bundle_id, None);

        assert_eq!(list[1].name, "", "facade fills the display name later");
        assert_eq!(list[1].is_output_active, None);
        assert_eq!(list[2].name, "direct");
    }

    #[test]
    fn build_falls_back_to_client_name() {
        let mut snap = RegistrySnapshot::default();
        snap.client_pid
            .insert(40, ClientEntry::from_props(None, Some("10"), None));
        snap.client_name.insert(40, "mpv".into());
        snap.nodes.insert(1, node(Some(40), None, None));
        let list = build_process_list(&snap, |_| None).unwrap();
        assert_eq!(list[0].name, "mpv");
        assert_eq!(list[0].executable, None);
    }

    #[test]
    fn build_pulse_pid_decision_table() {
        for (node_api, client_api) in [
            (None, Some("pipewire-pulse")),
            (Some("pipewire-pulse"), None),
            (None, Some("pipewire")),
        ] {
            for app_pid in [
                Some("1028793"),
                None,
                Some("0"),
                Some("-1"),
                Some("invalid"),
                Some(""),
            ] {
                let mut snap = RegistrySnapshot::default();
                snap.client_pid
                    .insert(40, ClientEntry::from_props(None, Some("1584"), client_api));
                let mut output = node(Some(40), None, Some("Chromium"));
                update_node_info(
                    &mut output.entry,
                    true,
                    Some((app_pid, node_api)),
                    &snap.client_pid,
                );
                snap.nodes.insert(1, output);
                let result = build_process_list(&snap, |_| None);
                let expected = if app_pid == Some("1028793") {
                    vec![1028793]
                } else if node_api.is_some() || client_api == Some("pipewire-pulse") {
                    Vec::new()
                } else {
                    vec![1584]
                };
                let actual =
                    result.map(|list| list.iter().map(|process| process.pid).collect::<Vec<_>>());
                let expected = if expected.is_empty() {
                    Err(SnapshotError::MissingPid { node_id: 1 })
                } else {
                    Ok(expected.clone())
                };
                assert_eq!(
                    actual, expected,
                    "node API={node_api:?}, client API={client_api:?}, app PID={app_pid:?}"
                );
            }
        }
    }

    #[test]
    fn build_retains_native_pid_but_clears_missing_pulse_pid() {
        for (node_api, client_api, expected) in [
            (None, Some("pipewire"), vec![42]),
            (None, Some("pipewire-pulse"), Vec::new()),
            (Some("pipewire-pulse"), None, Vec::new()),
        ] {
            for props in [None, Some((None, None)), Some((Some("invalid"), None))] {
                let mut snap = RegistrySnapshot::default();
                snap.client_pid
                    .insert(40, ClientEntry::from_props(None, Some("7"), client_api));
                let mut output = node(Some(40), Some(42), None);
                output.entry.app_pid_from_info = true;
                output.entry.pulse_proxied = is_pulse_proxied(node_api);
                output.entry.info_seen = true;
                output.entry.props_seen = true;
                update_node_info(&mut output.entry, true, props, &snap.client_pid);
                snap.nodes.insert(1, output);
                let result = build_process_list(&snap, |_| None);
                let actual =
                    result.map(|list| list.iter().map(|process| process.pid).collect::<Vec<_>>());
                let expected = if expected.is_empty() {
                    Err(SnapshotError::MissingPid { node_id: 1 })
                } else {
                    Ok(expected.clone())
                };
                assert_eq!(
                    actual, expected,
                    "node API={node_api:?}, client API={client_api:?}, props={props:?}"
                );
            }
        }
    }

    #[test]
    fn build_rejects_stale_bound_pid_when_pulse_client_arrives_late() {
        for props in [Some((None, None)), None] {
            let mut snap = RegistrySnapshot::default();
            let mut output = node(Some(40), None, Some("app"));
            update_node_info(
                &mut output.entry,
                true,
                Some((Some("42"), None)),
                &snap.client_pid,
            );
            snap.nodes.insert(1, output);
            assert_eq!(build_process_list(&snap, |_| None).unwrap()[0].pid, 42);

            let output = snap.nodes.get_mut(&1).expect("output node exists");
            update_node_info(&mut output.entry, true, props, &snap.client_pid);
            assert_eq!(output.entry.app_pid, Some(42));
            assert!(!output.entry.app_pid_from_info);
            assert_eq!(build_process_list(&snap, |_| None).unwrap()[0].pid, 42);

            snap.client_pid.insert(
                40,
                ClientEntry::from_props(None, Some("7"), Some("pipewire-pulse")),
            );
            assert_eq!(
                build_process_list(&snap, |_| None).unwrap_err(),
                SnapshotError::MissingPid { node_id: 1 }
            );
        }
    }

    #[test]
    fn clean_exe_path_strips_deleted_marker() {
        assert_eq!(clean_exe_path("/usr/bin/pw-cat").as_deref(), Some("pw-cat"));
        assert_eq!(
            clean_exe_path("/opt/app/bin/player (deleted)").as_deref(),
            Some("player")
        );
    }

    #[test]
    fn non_empty_trims_and_drops_blank() {
        assert_eq!(non_empty(Some("  mpv ")).as_deref(), Some("mpv"));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn timeout_with_no_output_nodes_is_err_even_if_clients_arrived() {
        let mut snap = RegistrySnapshot::default();
        snap.client_pid
            .insert(40, ClientEntry::from_props(None, Some("1234"), None));
        snap.client_name.insert(40, "silent-client".into());
        let err = finish_snapshot(false, None, snap).expect_err("timeout + 0 nodes must be Err");
        assert_eq!(err, SnapshotError::Incomplete);
    }

    #[test]
    fn timeout_with_output_nodes_rejects_the_partial_list() {
        let mut snap = RegistrySnapshot::default();
        snap.nodes.insert(1, node(Some(40), None, Some("app")));
        assert_eq!(
            finish_snapshot(false, None, snap).unwrap_err(),
            SnapshotError::Incomplete
        );
    }

    #[test]
    fn complete_with_zero_nodes_is_empty_ok() {
        let snap = RegistrySnapshot::default();
        let got = finish_snapshot(true, None, snap).expect("in-time empty is Ok");
        assert!(got.nodes.is_empty());
    }

    #[test]
    fn read_executable_reads_own_process() {
        let exe = read_executable(std::process::id()).expect("own /proc entry is readable");
        assert!(!exe.is_empty());
    }

    #[test]
    fn complete_populated_snapshot_keeps_optional_metadata_absent() {
        let mut snap = RegistrySnapshot::default();
        snap.nodes.insert(1, node(None, Some(42), None));
        let snap = finish_snapshot(true, None, snap).unwrap();
        let list = build_process_list(&snap, |_| None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pid, 42);
        assert_eq!(list[0].executable, None);
        assert_eq!(list[0].is_output_active, None);
    }

    #[test]
    fn missing_mandatory_pid_rejects_entire_populated_snapshot() {
        let mut snap = RegistrySnapshot::default();
        snap.nodes.insert(1, node(None, Some(42), None));
        snap.nodes.insert(2, node(Some(77), None, Some("orphan")));
        assert_eq!(
            build_process_list(&snap, |_| None).unwrap_err(),
            SnapshotError::MissingPid { node_id: 2 }
        );
    }

    #[test]
    fn failed_sync_preserves_cause_even_after_done() {
        let mut snap = RegistrySnapshot::default();
        snap.nodes.insert(1, node(None, Some(42), None));
        let failure = SnapshotError::Query {
            operation: "second sync",
            cause: "disconnected".into(),
        };
        assert_eq!(
            finish_snapshot(true, Some(failure), snap).unwrap_err(),
            SnapshotError::Query {
                operation: "second sync",
                cause: "disconnected".into()
            }
        );
    }

    #[test]
    fn bind_failure_preserves_node_and_cause() {
        let failure = RefCell::new(None);
        record_failure(
            &failure,
            SnapshotError::Bind {
                node_id: 7,
                cause: "permission denied".into(),
            },
        );
        run_callback(&failure, "node info", || panic!("later failure"));
        assert_eq!(
            finish_snapshot(true, failure.take(), RegistrySnapshot::default()).unwrap_err(),
            SnapshotError::Bind {
                node_id: 7,
                cause: "permission denied".into()
            }
        );
    }

    #[test]
    fn callback_failure_rejects_populated_snapshot() {
        let failure = RefCell::new(None);
        let mut snap = RegistrySnapshot::default();
        run_callback(&failure, "registry global", || {
            snap.nodes.insert(1, node(None, Some(42), None));
            panic!("synthetic callback failure");
        });
        assert_eq!(
            finish_snapshot(true, failure.take(), snap).unwrap_err(),
            SnapshotError::Callback {
                callback: "registry global",
                cause: "callback rejected".into(),
            }
        );
    }

    #[test]
    fn core_error_rejects_even_an_empty_completed_snapshot() {
        let error = SnapshotError::Core {
            object_id: 9,
            code: -5,
            cause: "I/O failure".into(),
        };
        assert_eq!(
            finish_snapshot(true, Some(error), RegistrySnapshot::default())
                .unwrap_err()
                .to_string(),
            format!(
                "pipewire object 9 error -5: {}",
                std::io::Error::from_raw_os_error(5)
            )
        );
    }

    #[test]
    fn error_display_omits_untrusted_core_and_panic_text() {
        let private_metadata = "private application name and executable path";
        for error in [
            SnapshotError::Core {
                object_id: 9,
                code: -5,
                cause: private_metadata.into(),
            },
            SnapshotError::Callback {
                callback: "registry global",
                cause: private_metadata.into(),
            },
        ] {
            let message = error.to_string();
            assert!(message.starts_with("pipewire"));
            assert!(!message.contains(private_metadata));
        }
    }
}
