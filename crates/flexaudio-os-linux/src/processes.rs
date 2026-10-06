//! Enumerate capturable processes from the PipeWire registry.
//!
//! A short-lived main loop collects Client PIDs, names and `client.api`, plus
//! output Node ownership, names and protocol provenance. Bound node info
//! supplies application PIDs and Running/Idle state.
//!
//! PID resolution shares [`resolve_node_pid`](crate::resolve_node_pid) with
//! process capture. Pulse nodes require a valid app PID from bound node info;
//! unresolved nodes are omitted instead of being listed under the proxy's PID.
//!
//! Get executable names from the kernel's `/proc/<pid>/exe`, not PipeWire's self-reported value
//! (fall back to `/proc/<pid>/comm` if unreadable).
//!
//! # Deadline
//! Use the same [`LIST_DEADLINE`] for the connection (`connect`) and registry round trip. Always
//! return, even if there is no response. If the deadline expires after collecting any output nodes,
//! return those nodes in `Ok` (their playback state is unknown, so use `None`). If no output nodes
//! were collected, return `Err` (do not treat an empty list containing only Clients as "available,
//! but none now"). If the request completes on time and there really are no nodes, return `Ok([])`.
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

/// Deadline for connection + registry round trip. Return `Ok` if anything was collected by then;
/// otherwise return `Err`.
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

/// List processes with an audio output stream (`Stream/Output/Audio`) as a raw list.
///
/// Returns [`Error::Backend`] if it cannot connect to PipeWire (daemon missing, `XDG_RUNTIME_DIR`
/// unset, etc.) or if no output nodes are collected before the deadline. If the deadline expires
/// after collecting any output nodes, return those nodes in `Ok` (their playback state is unknown,
/// so use `None`). If it completes on time and there are truly no nodes, return `Ok([])`. Process
/// capture is unavailable in the same environment, so an `Err`, rather than an empty list, signals
/// that process capture cannot be used here.
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    let snapshot = collect_snapshot().map_err(Error::Backend)?;
    Ok(build_process_list(&snapshot, read_executable))
}

/// Convert collection results to a raw list of [`ProcessInfo`]. Skip nodes whose PID cannot be resolved.
///
/// Display name preference: node `application.name`, then Client `application.name` (empty if neither
/// exists; the facade fills in an executable name, etc.). Sort deterministically by node ID.
fn build_process_list(
    snapshot: &RegistrySnapshot,
    executable_of: impl Fn(u32) -> Option<String>,
) -> Vec<ProcessInfo> {
    let mut node_ids: Vec<u32> = snapshot.nodes.keys().copied().collect();
    node_ids.sort_unstable();

    let mut out = Vec::with_capacity(node_ids.len());
    for node_id in node_ids {
        let node = &snapshot.nodes[&node_id];
        let Some(pid) = resolve_node_pid(&node.entry, &snapshot.client_pid) else {
            continue;
        };
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
    out
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

/// Read the PipeWire registry for one round trip. Returns `Err(String)` on failure (does not panic).
///
/// Create, use, and drop the `!Send` `MainLoop`/`Context`/`Core`/`Registry`/`Node` proxies only
/// inside this function. The facade calls it from a dedicated thread.
///
/// Wait for completion with the same two-phase sync→done barrier as `enumerate_pw` (phase 1 collects
/// all globals; phase 2 waits for info/state from bound nodes). A deadline timer also guarantees the
/// loop exits.
fn collect_snapshot() -> std::result::Result<RegistrySnapshot, String> {
    pw_init_once();
    let started = std::time::Instant::now();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    // The connection is also inside the deadline. connect itself cannot be interrupted, so if it
    // returns after the deadline, stop without waiting for the registry. If it hangs, the facade's
    // 3-second limit (single-flight) releases the caller.
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    if started.elapsed() >= LIST_DEADLINE {
        return Err(format!(
            "pipewire connect did not finish within {} ms",
            LIST_DEADLINE.as_millis()
        ));
    }
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    let snapshot = Rc::new(RefCell::new(RegistrySnapshot::default()));
    // Storage for bound node proxies and listeners (dropping it ends info subscriptions).
    type BoundNode = (pw::node::Node, pw::node::NodeListener);
    let bound_nodes: Rc<RefCell<Vec<BoundNode>>> = Rc::new(RefCell::new(Vec::new()));

    let snapshot_for_global = snapshot.clone();
    let registry_for_global = registry.clone();
    let bound_for_global = bound_nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // A panic across FFI is UB, so wrap the body in catch_unwind.
            let _ = catch_unwind(AssertUnwindSafe(|| {
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

                        // Bind for state and app PID; unresolved Pulse nodes are omitted.
                        let node: pw::node::Node = match registry_for_global.bind(global) {
                            Ok(node) => node,
                            Err(_) => return,
                        };
                        let snapshot_for_info = snapshot_for_global.clone();
                        let node_id = global.id;
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                // Catch unwinding at the FFI boundary; state() may
                                // panic on an error string containing invalid UTF-8.
                                let _ = catch_unwind(AssertUnwindSafe(|| {
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
                                }));
                            })
                            .register();
                        bound_for_global.borrow_mut().push((node, listener));
                    }
                    _ => {}
                }
            }));
        })
        .register();

    // Two-phase sync→done barrier (same as enumerate_pw).
    let done = Rc::new(Cell::new(false));
    let stage = Rc::new(Cell::new(0u8));
    let pending = core
        .sync(0)
        .map_err(|e| format!("pipewire sync failed: {e}"))?;
    let pending = Rc::new(Cell::new(pending.seq()));

    let done_for_cb = done.clone();
    let stage_for_cb = stage.clone();
    let pending_for_cb = pending.clone();
    let loop_for_cb = main_loop.clone();
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
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
                        _ => {
                            // Cannot start phase 2: state is unknown, so return what was collected.
                            done_for_cb.set(true);
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
            .map_err(|e| format!("arm pipewire deadline timer failed: {e}"))?;
        Some(deadline)
    };

    while !done.get() && !timed_out.get() {
        main_loop.run();
    }

    finish_snapshot(done.get(), snapshot.take())
}

/// Finish registry collection. `complete` is true if PipeWire's done arrived before the deadline.
/// If the deadline expired with no output nodes, return Err (do not turn an empty list of Clients
/// only into `Ok([])`, which means "available, but none now"). If it completed on time with truly no
/// nodes, return an empty `Ok` snapshot.
fn finish_snapshot(
    complete: bool,
    collected: RegistrySnapshot,
) -> std::result::Result<RegistrySnapshot, String> {
    if !complete && collected.nodes.is_empty() {
        return Err(format!(
            "pipewire registry did not answer within {} ms",
            LIST_DEADLINE.as_millis()
        ));
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
        // Skip a node whose PID cannot be resolved (unknown Client).
        snap.nodes.insert(103, node(Some(77), None, Some("orphan")));

        let list = build_process_list(&snap, |pid| Some(format!("exe-{pid}")));
        let pids: Vec<u32> = list.iter().map(|p| p.pid).collect();
        assert_eq!(
            pids,
            vec![1234, 5678, 999],
            "sorted by node id, orphan dropped"
        );

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
        let list = build_process_list(&snap, |_| None);
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
                let list = build_process_list(&snap, |_| None);
                let expected = if app_pid == Some("1028793") {
                    vec![1028793]
                } else if node_api.is_some() || client_api == Some("pipewire-pulse") {
                    Vec::new()
                } else {
                    vec![1584]
                };
                assert_eq!(
                    list.iter().map(|process| process.pid).collect::<Vec<_>>(),
                    expected,
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
                let list = build_process_list(&snap, |_| None);
                assert_eq!(
                    list.iter().map(|process| process.pid).collect::<Vec<_>>(),
                    expected,
                    "node API={node_api:?}, client API={client_api:?}, props={props:?}"
                );
            }
        }
    }

    #[test]
    fn build_omits_stale_bound_pid_when_pulse_client_arrives_late() {
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
            assert_eq!(build_process_list(&snap, |_| None)[0].pid, 42);

            let output = snap.nodes.get_mut(&1).expect("output node exists");
            update_node_info(&mut output.entry, true, props, &snap.client_pid);
            assert_eq!(output.entry.app_pid, Some(42));
            assert!(!output.entry.app_pid_from_info);
            assert_eq!(build_process_list(&snap, |_| None)[0].pid, 42);

            snap.client_pid.insert(
                40,
                ClientEntry::from_props(None, Some("7"), Some("pipewire-pulse")),
            );
            assert!(build_process_list(&snap, |_| None).is_empty());
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
        let err = finish_snapshot(false, snap).expect_err("timeout + 0 nodes must be Err");
        assert!(
            err.contains("did not answer"),
            "timeout error should mention the deadline, got {err}"
        );
    }

    #[test]
    fn timeout_with_output_nodes_keeps_the_partial_list() {
        let mut snap = RegistrySnapshot::default();
        snap.nodes.insert(1, node(Some(40), None, Some("app")));
        let got = finish_snapshot(false, snap).expect("timeout + some nodes is Ok");
        assert_eq!(got.nodes.len(), 1);
    }

    #[test]
    fn complete_with_zero_nodes_is_empty_ok() {
        let snap = RegistrySnapshot::default();
        let got = finish_snapshot(true, snap).expect("in-time empty is Ok");
        assert!(got.nodes.is_empty());
    }

    #[test]
    fn read_executable_reads_own_process() {
        let exe = read_executable(std::process::id()).expect("own /proc entry is readable");
        assert!(!exe.is_empty());
    }

    /// Never panics, whether PipeWire is present or not; returns either `Ok` or `Err(Backend)`.
    #[test]
    fn list_processes_is_graceful() {
        let started = std::time::Instant::now();
        match list_processes() {
            Ok(list) => {
                for p in &list {
                    assert_ne!(p.pid, 0);
                }
            }
            Err(Error::Backend(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
        assert!(
            started.elapsed() < LIST_DEADLINE + Duration::from_secs(1),
            "enumeration must be bounded by the deadline"
        );
    }
}
