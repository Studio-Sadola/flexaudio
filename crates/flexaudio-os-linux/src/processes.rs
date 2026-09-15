//! 録れるプロセスの列挙（[`list_processes`]）— PipeWire レジストリから。
//!
//! 短命の `MainLoop` を 1 本回してレジストリを 1 往復ぶん読み、
//! - Client global → `pipewire.sec.pid`（デーモンがソケット資格情報から付与＝詐称不可）と
//!   `application.name`
//! - `media.class == "Stream/Output/Audio"` の Node global → `client.id` と
//!   `application.name`（Node を bind して状態 Running/Idle も受ける）
//!
//! を集める。PID の解決はプロセス別キャプチャ（[`PwProcessBackend`](crate::PwProcessBackend)）
//! と同じ [`resolve_node_pid`](crate::resolve_node_pid)（node → client.id → Client の PID）
//! を使うので、ここに出た PID はそのまま `target_pid` に渡して録れる。
//!
//! 実行ファイル名は PipeWire の自己申告でなくカーネルの `/proc/<pid>/exe`
//! （読めなければ `/proc/<pid>/comm`）から取る。
//!
//! # 上限時間
//! レジストリの往復は通常すぐ終わるが、ループに期限タイマー（[`LIST_DEADLINE`]）を
//! 仕掛けてあり、応答が無くても必ず戻る（期限切れは `Err`）。
//!
//! 返すのは生リスト（同じ PID のノードが複数あれば重複する）で、重複統合・自プロセス
//! 除外・並べ替えは facade が行う。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::Duration;

use flexaudio_core::process_list::executable_basename;
use flexaudio_core::types::{Error, ProcessInfo, Result};

use pipewire as pw;

use crate::{pw_init_once, resolve_node_pid, NodeEntry};

/// レジストリ往復の期限。これを過ぎたら打ち切って `Err` を返す。
const LIST_DEADLINE: Duration = Duration::from_millis(2_000);

/// プロセス別キャプチャがリンク対象にするアプリ出力ノードの `media.class`。
const OUTPUT_STREAM_CLASS: &str = "Stream/Output/Audio";

/// レジストリから集めたアプリ出力ノード 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputNode {
    /// PID 解決用（プロセス別キャプチャと同じ形）。
    entry: NodeEntry,
    /// ノードの `application.name`（アプリの自己申告・表示用）。
    app_name: Option<String>,
    /// ノード状態が Running か（bind したノードの info が届いたときだけ `Some`）。
    running: Option<bool>,
}

/// レジストリ 1 往復ぶんの収集結果（PipeWire 非依存・テストで組み立てられる）。
#[derive(Debug, Default)]
struct RegistrySnapshot {
    /// Client global id → その Client の `pipewire.sec.pid`。
    client_pid: HashMap<u32, u32>,
    /// Client global id → その Client の `application.name`。
    client_name: HashMap<u32, String>,
    /// Node global id → アプリ出力ノード。
    nodes: HashMap<u32, OutputNode>,
}

/// 音声出力ストリーム（`Stream/Output/Audio`）を持つプロセスを列挙する（生リスト）。
///
/// PipeWire に接続できない（デーモン不在・`XDG_RUNTIME_DIR` 未設定など）ときや、
/// 期限内にレジストリが応答しないときは [`Error::Backend`]。プロセス別キャプチャも同じ
/// 環境では使えないので、`Err` は「この環境ではプロセス別キャプチャ不可」の合図になる。
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    let snapshot = collect_snapshot().map_err(Error::Backend)?;
    Ok(build_process_list(&snapshot, read_executable))
}

/// 収集結果を [`ProcessInfo`] の生リストへ写す。PID を解決できないノードは飛ばす。
///
/// 表示名はノードの `application.name` → Client の `application.name` の順（どちらも
/// 無ければ空。facade が実行ファイル名などで補う）。並びはノード id 順で決定的。
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

/// `/proc/<pid>/exe` のベース名（置き換え済みバイナリの ` (deleted)` は落とす）。
/// 読めなければ（他ユーザーのプロセス等）`/proc/<pid>/comm`。どちらも駄目なら `None`。
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

/// `/proc/<pid>/exe` のリンク先からベース名を取る。
fn clean_exe_path(path: &str) -> Option<String> {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    executable_basename(path)
}

/// 空でない props 値だけを `String` にする。
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// PipeWire レジストリを 1 往復ぶん読む本体。失敗は `Err(String)`（panic しない）。
///
/// `MainLoop`/`Context`/`Core`/`Registry`/`Node` プロキシ（いずれも `!Send`）は
/// この関数内だけで生成・実行・破棄する。facade が専用スレッドから呼ぶ。
///
/// 完了は `enumerate_pw` と同じ二段 sync→done バリアで待つ（1 段目で global が出揃い、
/// 2 段目で bind したノードの info＝状態が届く）。加えて期限タイマーでループを必ず抜ける。
fn collect_snapshot() -> std::result::Result<RegistrySnapshot, String> {
    pw_init_once();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| format!("create pipewire main loop failed: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| format!("create pipewire context failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to pipewire daemon failed (is PipeWire running?): {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("get pipewire registry failed: {e}"))?;

    let snapshot = Rc::new(RefCell::new(RegistrySnapshot::default()));
    // bind したノードのプロキシとリスナの保管庫（drop すると info の購読が切れる）。
    type BoundNode = (pw::node::Node, pw::node::NodeListener);
    let bound_nodes: Rc<RefCell<Vec<BoundNode>>> = Rc::new(RefCell::new(Vec::new()));

    let snapshot_for_global = snapshot.clone();
    let registry_for_global = registry.clone();
    let bound_for_global = bound_nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            // FFI 越えの panic は UB なので本体を catch_unwind で包む。
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

                        // 状態（Running/Idle/Suspended）は global props に無いので、ノードを
                        // bind して info を受ける。bind できなくても列挙自体は続ける（不明扱い）。
                        let node: pw::node::Node = match registry_for_global.bind(global) {
                            Ok(node) => node,
                            Err(_) => return,
                        };
                        let snapshot_for_info = snapshot_for_global.clone();
                        let node_id = global.id;
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                // info コールバックも FFI 越え。state() は不正な UTF-8 の
                                // エラー文字列で panic し得るので必ず包む。
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

    // 二段 sync→done バリア（enumerate_pw と同じ）。
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
                    // 1 段目完了（global が出揃った）→ bind したノードの info を待つ 2 段目。
                    stage_for_cb.set(1);
                    let second = core_weak.upgrade().map(|core| core.sync(0));
                    match second {
                        Some(Ok(p)) => pending_for_cb.set(p.seq()),
                        _ => {
                            // 2 段目を打てない: 状態は不明のまま、集めた分で終える。
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

    // 期限タイマー。レジストリが応答しなくてもループを必ず抜ける。
    let timed_out = Rc::new(Cell::new(false));
    let timed_out_for_timer = timed_out.clone();
    let loop_for_timer = main_loop.clone();
    let deadline = main_loop.loop_().add_timer(move |_expirations| {
        timed_out_for_timer.set(true);
        loop_for_timer.quit();
    });
    deadline
        .update_timer(Some(LIST_DEADLINE), None)
        .into_result()
        .map_err(|e| format!("arm pipewire deadline timer failed: {e}"))?;

    while !done.get() && !timed_out.get() {
        main_loop.run();
    }
    if !done.get() {
        return Err(format!(
            "pipewire registry did not answer within {} ms",
            LIST_DEADLINE.as_millis()
        ));
    }

    let collected = snapshot.take();
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
        // client 40 のノード（ノード名あり・Running）。
        snap.nodes.insert(
            100,
            OutputNode {
                running: Some(true),
                ..node(Some(40), None, Some("Firefox Audio"))
            },
        );
        // client 41 のノード（名前はノードに無く Client にも無い）。
        snap.nodes.insert(101, node(Some(41), None, None));
        // ノード自身に PID が載る構成（Client 不要）。
        snap.nodes
            .insert(102, node(None, Some(999), Some("direct")));
        // PID を解決できないノード（Client 不明）は飛ばす。
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
    fn read_executable_reads_own_process() {
        let exe = read_executable(std::process::id()).expect("own /proc entry is readable");
        assert!(!exe.is_empty());
    }

    /// PipeWire の有無にかかわらず panic せず、`Ok` か `Err(Backend)` のどちらかで戻る。
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
