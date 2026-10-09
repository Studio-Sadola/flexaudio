#![allow(dead_code, unused_imports)]
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::{mpsc, atomic::{AtomicBool, Ordering}};
use std::rc::Rc;
use flexaudio_core::types::ProcessMode;
use std::panic::{catch_unwind, AssertUnwindSafe};
use flexaudio_core::backend::RawSink;
use flexaudio_core::clock::monotonic_now_ns;
use flexaudio_core::types::{DeviceEvent, DeviceInfo, Error, Result, SourceKind};
// Typed SPA/stream fixtures, not native binding generation. Only byte-buffer
// handling and callback scheduling are under test, not the POD parser or daemon.
mod spa {
    pub mod pod { pub struct Pod; }
    pub mod param {
        pub enum ParamType { Format }
        impl ParamType { pub fn as_raw(&self) -> u32 { 4 } }
        pub mod format {
            #[derive(PartialEq)] pub enum MediaType { Audio }
            #[derive(PartialEq)] pub enum MediaSubtype { Raw }
        }
        pub mod format_utils {
            pub fn parse_format(_: &super::super::pod::Pod) -> std::result::Result<(super::format::MediaType, super::format::MediaSubtype), ()> {
                Ok((super::format::MediaType::Audio, super::format::MediaSubtype::Raw))
            }
        }
        pub mod audio {
            #[derive(Clone, Copy)] pub enum AudioFormat { F32LE }
            pub struct AudioInfoRaw { pub rate: u32, pub channels: u32 }
            impl AudioInfoRaw {
                pub fn new() -> Self { Self { rate: 0, channels: 0 } }
                pub fn set_format(&mut self, _: AudioFormat) {}
                pub fn set_rate(&mut self, rate: u32) { self.rate = rate; }
                pub fn set_channels(&mut self, channels: u32) { self.channels = channels; }
                pub fn parse(&mut self, _: &super::super::pod::Pod) -> std::result::Result<(), ()> { Err(()) }
            }
        }
    }
}
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
const NATIVE_CHANNELS: u16 = 2;
const PROC_SCRATCH_CAP: usize = 96_000;
const MAX_WATCH_EVENTS: usize = 1024;
thread_local! {
    static PROC_SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    static ENUM_FAILURE: Cell<bool> = const { Cell::new(false) };
}
mod pw {
    pub(crate) use crate::spa;
    pub mod constants { pub const ID_ANY: u32 = u32::MAX; }
    pub mod keys {
        pub const LINK_OUTPUT_NODE: &str = "out_node";
        pub const LINK_OUTPUT_PORT: &str = "out_port";
        pub const LINK_INPUT_NODE: &str = "in_node";
        pub const LINK_INPUT_PORT: &str = "in_port";
    }
    pub mod link { #[derive(Default)] pub struct Link; }
    pub mod core {
        pub const PW_ID_CORE: u32 = 0;
        use std::cell::{Cell, RefCell};
        pub struct CoreRc { pub fail: Cell<bool>, pub calls: Cell<usize>, pub failures: Cell<usize> }
        impl CoreRc {
            pub fn create_object<T: Default>(&self, _: &str, _: &Vec<String>) -> std::result::Result<T, &'static str> {
                self.calls.set(self.calls.get() + 1);
                if self.fail.get() {
                    self.failures.set(self.failures.get() + 1);
                    Err("injected link-factory refusal")
                } else { Ok(T::default()) }
            }
        }
    }
    pub mod channel {
        pub struct Sender<T>(std::marker::PhantomData<T>);
        pub struct Receiver<T>(std::marker::PhantomData<T>);
        pub fn channel<T>() -> (Sender<T>, Receiver<T>) { (Sender(std::marker::PhantomData), Receiver(std::marker::PhantomData)) }
        impl<T> Sender<T> { pub fn send(&self, _: T) -> std::result::Result<(), ()> { Ok(()) } }
        impl<T> Receiver<T> { pub fn attach(&self, _: &super::super::FakeLoop, _: impl FnMut(T)) {} }
    }
    pub mod stream {
        use std::cell::RefCell;
        pub struct Chunk { pub size: u32, pub offset: u32, pub stride: i32, pub flags: u32 }
        impl Chunk { pub fn size(&self) -> u32 { self.size } pub fn offset(&self) -> u32 { self.offset } }
        pub struct Data { pub chunk: Chunk, pub bytes: Vec<u8> }
        impl Data { pub fn chunk(&self) -> &Chunk { &self.chunk } pub fn data(&mut self) -> Option<&mut [u8]> { Some(&mut self.bytes) } }
        pub struct Buffer { pub datas: Vec<Data> }
        impl Buffer { pub fn datas_mut(&mut self) -> &mut [Data] { &mut self.datas } }
        pub struct StreamRc { pub id: u32, pub queued: RefCell<Option<Buffer>> }
        type Process<U> = Box<dyn FnMut(&StreamRc, &mut U)>;
        type Param<U> = Box<dyn FnMut(&StreamRc, &mut U, u32, Option<&super::spa::pod::Pod>)>;
        pub struct StreamListener<U> { pub user_data: U, pub process: Option<Process<U>>, pub param: Option<Param<U>> }
        impl StreamRc {
            pub fn node_id(&self) -> u32 { self.id }
            pub fn dequeue_buffer(&self) -> Option<Buffer> { self.queued.borrow_mut().take() }
            pub fn add_local_listener_with_user_data<U>(&self, user_data: U) -> StreamListener<U> {
                StreamListener { user_data, process: None, param: None }
            }
        }
        impl<U> StreamListener<U> {
            pub fn param_changed(mut self, callback: impl FnMut(&StreamRc, &mut U, u32, Option<&super::spa::pod::Pod>) + 'static) -> Self {
                self.param = Some(Box::new(callback)); self
            }
            pub fn process(mut self, callback: impl FnMut(&StreamRc, &mut U) + 'static) -> Self {
                self.process = Some(Box::new(callback)); self
            }
            pub fn register(self) -> std::result::Result<Self, &'static str> { Ok(self) }
            pub fn fire(&mut self, stream: &StreamRc) { (self.process.as_mut().unwrap())(stream, &mut self.user_data); }
        }
    }
}
// Ignore only property keys, not values or planning; keys are not used by our
// refusal fixture. Production passes the same four computed IDs to create_object.
macro_rules! properties {
    ($($key:expr => $value:expr),* $(,)?) => { vec![$($value),*] };
}
struct NodeEntry { pid: u32, n_output_ports: Option<u32> }
struct ClientEntry;
struct PortEntry { node_id: u32, direction: String, channel: String }
enum PidSelect { Include(u32), Exclude(HashSet<u32>) }
impl PidSelect {
    fn selects_node(&self, entry: &NodeEntry, _: &HashMap<u32, ClientEntry>) -> bool {
        match self { Self::Include(pid) => entry.pid == *pid, Self::Exclude(pids) => !pids.contains(&entry.pid) }
    }
}
struct UserData { format: spa::param::audio::AudioInfoRaw, sink: RawSink }
// LIVE_PAIRING
// LIVE_LINKING
// LIVE_CAPTURE
fn enumerate_pw() -> std::result::Result<Vec<DeviceInfo>, String> {
    if ENUM_FAILURE.get() { Err("injected registry query failure".into()) } else { Ok(Vec::new()) }
}
// LIVE_LIST_DEVICES
// LIVE_ENQUEUE
// LIVE_JSON_NAME

#[derive(Clone)]
struct FakeLoop;
thread_local! {
    static READY_RX: RefCell<Option<mpsc::Receiver<std::result::Result<(), String>>>> = const { RefCell::new(None) };
    static READY_EARLY: Cell<bool> = const { Cell::new(false) };
    static SYNC_FAILURE: Cell<bool> = const { Cell::new(false) };
}
impl FakeLoop {
    fn loop_(&self) -> &Self { self }
    fn quit(&self) {}
    fn run(&self) { READY_RX.with(|rx| READY_EARLY.set(matches!(rx.borrow().as_ref().unwrap().try_recv(), Ok(Ok(()))))); }
}
struct Terminate;
fn setup_pw(_: Option<String>, _: RawSink) -> std::result::Result<(FakeLoop, (), ()), String> { Ok((FakeLoop, (), ())) }
// LIVE_READINESS
#[test]
#[ignore = "repro: F03"]
fn repro_p7_early_readiness() {
    let (tx, rx) = mpsc::channel(); READY_RX.with(|r| *r.borrow_mut() = Some(rx));
    let (_, stop) = pw::channel::channel(); let (producer, _) = flexaudio_core::raw_ring(16);
    run_pw_loop(None, RawSink::new(producer, 48_000, 2), stop, &tx);
    assert!(!READY_EARLY.get(), "F03: successful readiness observable before loop runs negotiation");
}
#[test]
fn repro_p7_early_readiness_control() {
    // A departed caller causes the production loop to skip run/negotiation.
    READY_EARLY.set(false);
    let (tx, rx) = mpsc::channel(); drop(rx);
    let (_, stop) = pw::channel::channel(); let (producer, _) = flexaudio_core::raw_ring(16);
    run_pw_loop(None, RawSink::new(producer, 48_000, 2), stop, &tx);
    assert!(!READY_EARLY.get());
}
mod thread {
    pub use std::thread::JoinHandle;
    thread_local! { pub static FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    pub struct Builder;
    impl Builder {
        pub fn new() -> Self { Self }
        pub fn name(self, _: String) -> Self { self }
        pub fn spawn(self, work: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
            if FAIL.get() { Err(std::io::Error::other("injected OS spawn failure")) } else { std::thread::Builder::new().spawn(work) }
        }
    }
}
struct ProcessOwner {
    running: Arc<AtomicBool>, target_pid: u32, mode: ProcessMode,
    stop_tx: Option<pw::channel::Sender<Terminate>>, handle: Option<thread::JoinHandle<()>>,
}
fn run_pw_process_loop(_: PidSelect, _: RawSink, _: pw::channel::Receiver<Terminate>, ready: &mpsc::Sender<std::result::Result<(), String>>) { ready.send(Ok(())).unwrap(); }
impl ProcessOwner {
    // LIVE_PROCESS_START
    // LIVE_SYSTEM_STOP
}
impl flexaudio_core::CaptureBackend for ProcessOwner {
    fn native_format(&self) -> (u32, u16) { (48_000, 2) }
    fn start(&mut self, sink: RawSink) -> Result<()> { ProcessOwner::start(self, sink) }
    fn stop(&mut self) { ProcessOwner::stop(self); }
    // Matches the native adapter: no poll_event override, real core default.
}
fn process_owner() -> ProcessOwner { ProcessOwner { running: Arc::new(AtomicBool::new(false)), target_pid: 42, mode: ProcessMode::Include, stop_tx: None, handle: None } }
fn sink() -> RawSink { let (producer, _) = flexaudio_core::raw_ring(16); RawSink::new(producer, 48_000, 2) }
#[test]
#[ignore = "repro: F12"]
fn repro_p7_spawn_rollback() {
    thread::FAIL.set(true); let mut owner = process_owner();
    let first = owner.start(sink()); let second = owner.start(sink());
    assert!(first.is_err());
    assert!(second.is_err(), "F12: first spawn failed; retry returned {second:?}, running={}, handle={}", owner.running.load(Ordering::SeqCst), owner.handle.is_some());
}
#[test]
fn repro_p7_spawn_rollback_control() {
    thread::FAIL.set(false); let mut owner = process_owner();
    owner.start(sink()).unwrap(); assert!(owner.handle.is_some()); owner.stop();
    assert!(!owner.running.load(Ordering::SeqCst)); assert!(owner.handle.is_none());
}
#[test]
#[ignore = "repro: F37"]
fn repro_p7_join_failure() {
    let mut owner = process_owner();
    owner.handle = Some(std::thread::spawn(|| panic!("injected PipeWire owner panic")));
    owner.stop();
    let event = flexaudio_core::CaptureBackend::poll_event(&mut owner);
    assert!(event.is_some(), "F37: PipeWire owner join panic discarded; stop returned (), event=None");
}
#[test]
fn repro_p7_join_failure_control() {
    let mut owner = process_owner(); owner.handle = Some(std::thread::spawn(|| {})); owner.stop();
    assert!(owner.handle.is_none()); assert!(flexaudio_core::CaptureBackend::poll_event(&mut owner).is_none());
}
struct WatchQueue { events: Arc<Mutex<VecDeque<DeviceEvent>>> }
impl WatchQueue { // LIVE_POLL
}
fn watcher(poison: bool) -> WatchQueue {
    let events = Arc::new(Mutex::new(VecDeque::from([DeviceEvent::Removed { id: "mic".into() }])));
    if poison { let shared = events.clone(); let _ = std::thread::spawn(move || { let _lock = shared.lock().unwrap(); panic!("injected queue-owner panic"); }).join(); }
    WatchQueue { events }
}
#[test]
#[ignore = "repro: F27/M5"]
fn repro_p7_poisoned_queue() {
    let mut w = watcher(true);
    assert!(w.poll_event().is_some(), "F27/M5: queued removal exists behind poisoned lock; poll_event=None");
}
#[test]
fn repro_p7_poisoned_queue_control() {
    assert_eq!(watcher(false).poll_event(), Some(DeviceEvent::Removed { id: "mic".into() }));
}
struct WatchState { default_sink: Option<String>, default_source: Option<String>, initial_scan_done: bool }
fn default_change(value: Option<&str>) -> (Option<String>, usize) {
    let state_for_meta = Rc::new(RefCell::new(WatchState { default_sink: Some("old".into()), default_source: None, initial_scan_done: true }));
    let events_for_meta = Arc::new(Mutex::new(VecDeque::new()));
    let inspect_state = state_for_meta.clone(); let inspect_events = events_for_meta.clone();
    let callback = // LIVE_DEFAULT_CALLBACK
    ;
    callback(0u32, Some("default.audio.sink"), None::<&str>, value);
    let state = inspect_state.borrow().default_sink.clone(); let count = inspect_events.lock().unwrap().len(); (state, count)
}
#[test]
#[ignore = "repro: L2"]
fn repro_p7_default_clear() {
    let (state, count) = default_change(Some("{}"));
    assert!(state.is_none()); assert!(count > 0, "L2: default changed old -> None; queued events=0");
}
#[test]
fn repro_p7_default_clear_control() {
    assert_eq!(default_change(Some(r#"{"name":"new"}"#)), (Some("new".into()), 1));
}
#[derive(Clone, Copy)] struct Sequence(i32);
impl Sequence { fn seq(&self) -> i32 { self.0 } }
struct SyncCore;
impl SyncCore { fn sync(&self, _: u32) -> std::result::Result<Sequence, &'static str> { if SYNC_FAILURE.get() { Err("injected second sync refusal") } else { Ok(Sequence(2)) } } }
struct WeakSync;
impl WeakSync { fn upgrade(&self) -> Option<SyncCore> { Some(SyncCore) } }
fn typed_sync(callback: impl Fn(u32, Sequence)) -> impl Fn(u32, Sequence) { callback }
fn scan_sync(fail: bool) -> bool {
    SYNC_FAILURE.set(fail);
    let done_for_cb = Rc::new(Cell::new(false)); let inspect = done_for_cb.clone();
    let stage_for_cb = Rc::new(Cell::new(0)); let pending1_for_cb = Rc::new(Cell::new(1));
    let loop_for_cb = FakeLoop; let core_weak = WeakSync;
    let callback = typed_sync( // LIVE_SYNC_CALLBACK
    );
    callback(0, Sequence(1));
    if !fail { callback(0, Sequence(2)); }
    inspect.get()
}
#[test]
fn repro_p7_failed_sync_completion() {
    assert!(!scan_sync(true), "F24: refused second sync marked enumeration done=true");
}
#[test]
fn repro_p7_failed_sync_completion_control() { assert!(scan_sync(false)); }
const ENUMERATE_DEADLINE_MS: u128 = 2_000;
struct BlockingLoop { release: mpsc::Receiver<()>, done: Rc<Cell<bool>> }
impl BlockingLoop { fn run(&self) { self.release.recv().unwrap(); self.done.set(true); } }
fn scan_deadline(block: bool) -> bool {
    let (release_tx, release_rx) = mpsc::channel(); let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let done = Rc::new(Cell::new(!block));
        let main_loop = BlockingLoop { release: release_rx, done: done.clone() };
        // LIVE_DEADLINE
        finished_tx.send(()).unwrap();
    });
    let completed = finished_rx.recv_timeout(std::time::Duration::from_millis(2200)).is_ok();
    // Always unblock and join before asserting, including the failure case.
    if block { release_tx.send(()).unwrap(); }
    worker.join().unwrap(); completed
}
#[test]
#[ignore = "repro: F24"]
fn repro_p7_scan_deadline() {
    assert!(scan_deadline(true), "F24: 2000ms deadline exceeded; scan still blocked after 2200ms until external release");
}
#[test]
fn repro_p7_scan_deadline_control() { assert!(scan_deadline(false)); }
struct RawNode { media_class: String, node_name: String, description: String, rate: Option<u32>, channels: Option<u16> }
struct DeviceState { nodes: Vec<RawNode>, default_sink: Option<String>, default_source: Option<String> }
fn advertised_mic() -> DeviceInfo {
    let state = DeviceState { nodes: vec![RawNode { media_class: "Audio/Source".into(), node_name: "alsa_input.card0".into(), description: "USB microphone".into(), rate: Some(48_000), channels: Some(1) }], default_sink: None, default_source: None };
    let mut out = Vec::new();
    const NATIVE_RATE: u32 = 48_000;
    // LIVE_DEVICE_INFO
    out.pop().unwrap()
}
#[test]
fn repro_p7_mic_identity_control() {
    let mic = advertised_mic(); assert_eq!(mic.source_kind, SourceKind::Mic);
    assert_eq!(mic.id, "alsa_input.card0");
}
fn remove_port(f: &LinkFixture, id: u32) {
    let linked_for_remove = &f.linked; let nodes_for_remove = &f.nodes;
    let ports_for_remove = &f.ports; let client_pid_for_remove = &f.clients;
    let core_for_remove = &f.core; let stream_for_remove = &f.stream;
    let self_node_for_remove = &f.own;
    let select_for_remove = PidSelect::Include(42);
    let target_client_for_remove = RefCell::new(HashSet::<u32>::new());
    let bound_for_remove = RefCell::new(HashMap::<u32, ()>::new());
    let callback = // LIVE_REMOVE_CALLBACK
    ;
    callback(id);
}
#[test]
fn repro_p7_relink_gap_attempt() {
    let f = LinkFixture::new(1, false); f.link();
    remove_port(&f, 1001); assert!(f.linked.borrow().is_empty());
    f.ports.borrow_mut().insert(1001, PortEntry { node_id: 99, direction: "in".into(), channel: "FR".into() });
    f.link(); assert_eq!(f.linked.borrow().get(&1).unwrap().len(), 2);
    // Fake link creation is synchronous: this cannot establish actual lost PCM
    // or the gap duration between native unlink/relink completion.
}
#[test]
fn repro_p7_relink_gap_attempt_control() {
    let f = LinkFixture::new(1, false); f.link(); remove_port(&f, 123456);
    assert_eq!(f.linked.borrow().get(&1).unwrap().len(), 2);
}

struct LinkFixture {
    core: pw::core::CoreRc,
    stream: pw::stream::StreamRc,
    own: Cell<Option<u32>>,
    nodes: RefCell<HashMap<u32, NodeEntry>>,
    clients: RefCell<HashMap<u32, ClientEntry>>,
    ports: RefCell<HashMap<u32, PortEntry>>,
    linked: RefCell<HashMap<u32, Vec<pw::link::Link>>>,
}
impl LinkFixture {
    fn new(count: u32, fail: bool) -> Self {
        let mut nodes = HashMap::new();
        let mut ports = HashMap::new();
        for id in 1..=count {
            nodes.insert(id, NodeEntry { pid: 42, n_output_ports: Some(2) });
            for (channel, offset) in [("FL", 0), ("FR", 1)] {
                ports.insert(id * 10 + offset, PortEntry { node_id: id, direction: "out".into(), channel: channel.into() });
            }
        }
        for (channel, id) in [("FL", 1000), ("FR", 1001)] {
            ports.insert(id, PortEntry { node_id: 99, direction: "in".into(), channel: channel.into() });
        }
        Self { core: pw::core::CoreRc { fail: Cell::new(fail), calls: Cell::new(0), failures: Cell::new(0) },
            stream: pw::stream::StreamRc { id: 99, queued: RefCell::new(None) }, own: Cell::new(None),
            nodes: RefCell::new(nodes), clients: RefCell::new(HashMap::new()), ports: RefCell::new(ports), linked: RefCell::new(HashMap::new()) }
    }
    fn link(&self) {
        try_link(&self.core, &self.stream, &PidSelect::Include(42), &self.own,
                 &self.nodes, &self.clients, &self.ports, &self.linked);
    }
}
#[test]
fn repro_p7_multinode_include() {
    let f = LinkFixture::new(2, false);
    f.link(); f.link();
    assert_eq!(f.linked.borrow().len(), 2, "F29: two selected stereo nodes; only one linked even after reevaluation");
}
#[test]
fn repro_p7_multinode_include_control() {
    let f = LinkFixture::new(1, false); f.link();
    assert_eq!(f.linked.borrow().len(), 1);
    assert_eq!(f.core.calls.get(), 2);
}
#[test]
#[ignore = "repro: F30"]
fn repro_p7_link_refusal() {
    let f = LinkFixture::new(1, true); f.link();
    assert_eq!(f.core.failures.get(), 1, "failure injection must reach create_object");
    // try_link returns (), so the adapter can neither return the native refusal
    // nor report any per-source state. The caller sees only the unchanged map.
    assert!(!f.linked.borrow().is_empty(), "F30: injected link-factory refusal discarded; linked=0, try_link returned ()");
}
#[test]
fn repro_p7_link_refusal_control() {
    let f = LinkFixture::new(1, false); f.link();
    assert_eq!(f.core.failures.get(), 0);
    assert_eq!(f.linked.borrow().get(&1).unwrap().len(), 2);
}
#[test]
#[ignore = "repro: F15"]
fn repro_p7_multichannel_loss() {
    let output = [(1, "FL".into()), (2, "FR".into()), (3, "FC".into())];
    let input = [(10, "FL".into()), (11, "FR".into())];
    let pairs = pair_ports(&output, &input);
    let complete = link_plan_is_complete(Some(3), output.len(), input.len(), pairs.len(), 2);
    assert!(!complete || pairs.iter().any(|(out, _)| *out == 3), "F15: complete=true with center channel 3 omitted; pairs={pairs:?}");
}
#[test]
fn repro_p7_multichannel_loss_control() {
    let output = [(1, "FL".into()), (2, "FR".into())];
    let input = [(10, "FL".into()), (11, "FR".into())];
    assert_eq!(pair_ports(&output, &input), [(1, 10), (2, 11)]);
    assert!(link_plan_is_complete(Some(2), 2, 2, 2, 2));
}
#[test]
#[ignore = "repro: F23"]
fn repro_p7_query_failure() {
    ENUM_FAILURE.set(true);
    let result = list_devices();
    assert!(result.is_err(), "F23: injected registry query failure returned {result:?}");
}
#[test]
fn repro_p7_query_failure_control() {
    ENUM_FAILURE.set(false); assert_eq!(list_devices().unwrap(), vec![]);
}
fn process_fixture(bytes: Vec<u8>, stride: i32, flags: u32) -> (pw::stream::StreamRc, pw::stream::StreamListener<UserData>, flexaudio_core::raw_ring::RawConsumer) {
    let size = u32::try_from(bytes.len()).unwrap();
    let stream = pw::stream::StreamRc { id: 99, queued: RefCell::new(Some(pw::stream::Buffer { datas: vec![pw::stream::Data {
        chunk: pw::stream::Chunk { size, offset: 0, stride, flags }, bytes,
    }] })) };
    let (producer, consumer) = flexaudio_core::raw_ring(16);
    let mut format = spa::param::audio::AudioInfoRaw::new();
    format.set_format(spa::param::audio::AudioFormat::F32LE); format.set_rate(48_000); format.set_channels(2);
    let listener = add_capture_listener(&stream, UserData { format, sink: RawSink::new(producer, 48_000, 2) }).unwrap();
    (stream, listener, consumer)
}
#[test]
#[ignore = "repro: F32/M22"]
fn repro_p7_corrupt_buffer() {
    let (stream, mut listener, mut consumer) = process_fixture([0.25f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat(), 8, 1);
    listener.fire(&stream);
    let got = consumer.pop_slice(&mut [0.0; 16]);
    assert_eq!(got, 0, "F32/M22: CORRUPTED SPA chunk delivered as valid PCM");
}
#[test]
fn repro_p7_corrupt_buffer_control() {
    let (stream, mut listener, mut consumer) = process_fixture([0.25f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat(), 8, 0);
    listener.fire(&stream);
    let mut out = [0.0; 2]; assert_eq!(consumer.pop_slice(&mut out), 2); assert_eq!(out, [0.25, -0.25]);
}
#[test]
#[ignore = "repro: F32/M22"]
fn repro_p7_padded_stride() {
    let bytes = [0.25f32.to_le_bytes(), (-0.25f32).to_le_bytes(), 123.0f32.to_le_bytes(), 0.5f32.to_le_bytes(), (-0.5f32).to_le_bytes(), 123.0f32.to_le_bytes()].concat();
    let (stream, mut listener, mut consumer) = process_fixture(bytes, 12, 0);
    listener.fire(&stream); let mut out = [0.0; 16]; let got = consumer.pop_slice(&mut out);
    assert_eq!(&out[..got], &[0.25, -0.25, 0.5, -0.5], "F32/M22: padded stride read as samples");
}
#[test]
fn repro_p7_padded_stride_control() { repro_p7_corrupt_buffer_control(); }
#[test]
#[ignore = "repro: F33"]
fn repro_p7_callback_borrow_panic() {
    let (stream, mut listener, mut consumer) = process_fixture([0.25f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat(), 8, 0);
    PROC_SCRATCH.with(|cell| { let _borrow = cell.borrow_mut(); listener.fire(&stream); });
    let got = consumer.pop_slice(&mut [0.0; 2]);
    assert!(got > 0 || listener.user_data.sink.overflow_count() > 0, "F33: injected reentrant borrow panic swallowed; delivered=0, loss_count=0");
}
#[test]
fn repro_p7_callback_borrow_panic_control() { repro_p7_corrupt_buffer_control(); }
#[test]
#[ignore = "repro: F27"]
fn repro_p7_queue_overflow() {
    let events = Arc::new(Mutex::new(VecDeque::new()));
    for i in 0..=MAX_WATCH_EVENTS { enqueue_event(&events, DeviceEvent::Removed { id: i.to_string() }); }
    let q = events.lock().unwrap();
    assert_eq!(q.front(), Some(&DeviceEvent::Removed { id: "0".into() }), "F27: oldest removal silently lost; first remaining={:?}, length={}", q.front(), q.len());
}
#[test]
fn repro_p7_queue_overflow_control() {
    let events = Arc::new(Mutex::new(VecDeque::new()));
    for i in 0..MAX_WATCH_EVENTS { enqueue_event(&events, DeviceEvent::Removed { id: i.to_string() }); }
    let q = events.lock().unwrap(); assert_eq!(q.len(), MAX_WATCH_EVENTS);
    assert_eq!(q.front(), Some(&DeviceEvent::Removed { id: "0".into() }));
}
