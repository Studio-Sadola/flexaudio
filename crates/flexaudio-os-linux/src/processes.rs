//! Enumeration of capturable processes ([`list_processes`]), from the PipeWire registry.
//!
//! Runs one short-lived `MainLoop`, reads one registry round trip, and collects
//! - Client globals → `pipewire.sec.pid` (set by the daemon from the socket credentials, so it
//!   cannot be spoofed) and `application.name`
//! - Node globals with `media.class == "Stream/Output/Audio"` → `client.id` and
//!   `application.name` (the Node is bound so that its Running/Idle state is received too)
//!
//! PID resolution uses the same [`resolve_node_pid`](crate::resolve_node_pid) (node → client.id
//! → the Client's PID) as per-process capture ([`PwProcessBackend`](crate::PwProcessBackend)),
//! so any PID listed here can be passed as-is to `target_pid` and captured.
//!
//! The executable name comes from the kernel's `/proc/<pid>/exe` (or `/proc/<pid>/comm` if that
//! is unreadable), not from PipeWire's self-reported properties.
//!
//! # Time limit
//! Everything from the connection (`connect`) to the registry round trip is inside the same
//! deadline ([`LIST_DEADLINE`]). It always returns even without a response. If the deadline
//! expires but at least one output node has been collected, those are returned as `Ok` (with
//! `None` for nodes whose output activity is unknown). If the deadline expires with zero output
//! nodes, it returns `Err` (an empty list with only Clients collected is not reported as "usable
//! but nothing right now"). If it completes within the deadline with genuinely zero nodes, it
//! returns `Ok([])`.
//!
//! It returns the raw list (duplicated if several nodes share a PID); deduplication, own-process
//! exclusion, and sorting are done by the facade.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::Duration;

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{Error, ProcessInfo, Result};

use pipewire as pw;

use crate::{pw_init_once, resolve_node_pid, NodeEntry};

/// Deadline for connect + registry round trip. Past it, whatever was collected is `Ok`; empty is
/// `Err`.
const LIST_DEADLINE: Duration = Duration::from_millis(2_000);

/// `media.class` of the app output nodes that per-process capture links to.
const OUTPUT_STREAM_CLASS: &str = "Stream/Output/Audio";

/// One app output node collected from the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputNode {
    /// For PID resolution (same shape as per-process capture).
    entry: NodeEntry,
    /// The node's `application.name` (self-reported by the app; for display).
    app_name: Option<String>,
    /// Whether the node state is Running (`Some` only once the bound node's info has arrived).
    running: Option<bool>,
}

/// Result of one registry round trip (PipeWire-independent; tests can build it by hand).
#[derive(Debug, Default)]
struct RegistrySnapshot {
    /// Client global id → that Client's `pipewire.sec.pid`.
    client_pid: HashMap<u32, u32>,
    /// Client global id → that Client's `application.name`.
    client_name: HashMap<u32, String>,
    /// Node global id → app output node.
    nodes: HashMap<u32, OutputNode>,
}

/// Lists processes that have an audio output stream (`Stream/Output/Audio`) (raw list).
///
/// Returns [`Error::Backend`] when PipeWire is unreachable (no daemon, `XDG_RUNTIME_DIR` unset,
/// etc.) or when no output node was collected within the deadline. If the deadline expires but
/// at least one output node exists, those are returned as `Ok` (with `None` for nodes whose
/// output activity is unknown). If it completes within the deadline with genuinely zero nodes,
/// it returns `Ok([])`. Per-process capture is also unavailable in the same environments, so an
/// empty `Err` signals "per-process capture is not possible in this environment".
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    let snapshot = collect_snapshot().map_err(Error::Backend)?;
    Ok(build_process_list(&snapshot, read_executable))
}

/// Maps the collected result to a raw list of [`ProcessInfo`]. Nodes whose PID cannot be
/// resolved are skipped.
///
/// The display name is the node's `application.name`, then the Client's `application.name`
/// (empty if neither exists; the facade fills it in from the executable name etc.). The order is
/// deterministic, by node id.
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

/// Base name of `/proc/<pid>/exe` (the ` (deleted)` suffix of a replaced binary is dropped).
/// If unreadable (another user's process, etc.), `/proc/<pid>/comm`. `None` if both fail.
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

/// Takes the base name from the `/proc/<pid>/exe` link target.
fn clean_exe_path(path: &str) -> Option<String> {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    executable_basename(path)
}

/// Converts a props value to `String` only if it is non-empty.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The core that reads one PipeWire registry round trip. Failures are `Err(String)` (no panic).
///
/// The `MainLoop`/`Context`/`Core`/`Registry`/`Node` proxies (all `!Send`) are created, run, and
/// destroyed only inside this function. The facade calls it from a dedicated thread.
///
/// Completion is awaited with the same two-stage sync→done barrier as `enumerate_pw` (stage 1:
/// all globals have arrived; stage 2: the bound nodes' info, i.e. their state, has arrived). In
/// addition, a deadline timer guarantees the loop exits.
fn collect_snapshot() -> std::result::Result<RegistrySnapshot, String> {
    pw_init_once();
    let started = std::time::Instant::now();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    // The connection is also inside the deadline. connect itself cannot be interrupted, so if
    // the deadline has passed when it returns, give up without waiting on the registry. If it
    // hangs, the facade's 3-second limit (single-flight) releases the caller.
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
    // Storage for the bound nodes' proxies and listeners (dropping them ends the info
    // subscription).
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
                        let Some(pid) = props
                            .get(*pw::keys::SEC_PID)
                            .and_then(|s| s.parse::<u32>().ok())
                        else {
                            return;
                        };
                        let mut snap = snapshot_for_global.borrow_mut();
                        snap.client_pid.insert(global.id, pid);
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
                            app_pid: props
                                .get(*pw::keys::SEC_PID)
                                .and_then(|s| s.parse::<u32>().ok()),
                        };
                        snapshot_for_global.borrow_mut().nodes.insert(
                            global.id,
                            OutputNode {
                                entry,
                                app_name: non_empty(props.get(*pw::keys::APP_NAME)),
                                running: None,
                            },
                        );

                        // The state (Running/Idle/Suspended) is not in the global props, so bind
                        // the node and receive its info. Enumeration continues even if bind fails
                        // (treated as unknown).
                        let node: pw::node::Node = match registry_for_global.bind(global) {
                            Ok(node) => node,
                            Err(_) => return,
                        };
                        let snapshot_for_info = snapshot_for_global.clone();
                        let node_id = global.id;
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                // The info callback also crosses FFI. state() can panic on an
                                // error string with invalid UTF-8, so always wrap it.
                                let _ = catch_unwind(AssertUnwindSafe(|| {
                                    let running =
                                        matches!(info.state(), pw::node::NodeState::Running);
                                    if let Some(entry) =
                                        snapshot_for_info.borrow_mut().nodes.get_mut(&node_id)
                                    {
                                        entry.running = Some(running);
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

    // Two-stage sync→done barrier (same as enumerate_pw).
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
                    // Stage 1 done (all globals arrived) → stage 2 waits for the bound nodes' info.
                    stage_for_cb.set(1);
                    let second = core_weak.upgrade().map(|core| core.sync(0));
                    match second {
                        Some(Ok(p)) => pending_for_cb.set(p.seq()),
                        _ => {
                            // Cannot issue stage 2: finish with what was collected, leaving the
                            // state unknown.
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

    // Deadline timer, set to the time remaining after the connection. Guarantees the loop exits
    // even if the registry does not respond. The timer stays alive for the whole run().
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

/// Finalizes registry collection. `complete` is true when PipeWire's done arrived within the
/// deadline. If the deadline expired with zero output nodes, returns Err (an empty list with only
/// Clients collected is not reported as the "usable but nothing right now" `Ok([])`). If it
/// completed within the deadline with genuinely zero nodes, returns an empty snapshot as `Ok`.
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
            },
            app_name: name.map(str::to_string),
            running: None,
        }
    }

    #[test]
    fn build_resolves_pid_through_client_like_the_capture_backend() {
        let mut snap = RegistrySnapshot::default();
        snap.client_pid.insert(40, 1234);
        snap.client_name.insert(40, "Firefox".into());
        snap.client_pid.insert(41, 5678);
        // Node of client 40 (has a node name, Running).
        snap.nodes.insert(
            100,
            OutputNode {
                running: Some(true),
                ..node(Some(40), None, Some("Firefox Audio"))
            },
        );
        // Node of client 41 (no name on the node nor on the Client).
        snap.nodes.insert(101, node(Some(41), None, None));
        // Setup where the node itself carries the PID (no Client needed).
        snap.nodes
            .insert(102, node(None, Some(999), Some("direct")));
        // A node whose PID cannot be resolved (unknown Client) is skipped.
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
        snap.client_pid.insert(40, 10);
        snap.client_name.insert(40, "mpv".into());
        snap.nodes.insert(1, node(Some(40), None, None));
        let list = build_process_list(&snap, |_| None);
        assert_eq!(list[0].name, "mpv");
        assert_eq!(list[0].executable, None);
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
        snap.client_pid.insert(40, 1234);
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

    /// Never panics regardless of whether PipeWire is present; returns either `Ok` or
    /// `Err(Backend)`.
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
