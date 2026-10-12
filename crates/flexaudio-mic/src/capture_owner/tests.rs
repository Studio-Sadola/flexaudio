use super::*;
use flexaudio_core::CaptureBackend;

#[test]
fn authorized_owner_stop_and_drop_join_without_consent_monitoring() {
    for drop_backend in [false, true] {
        let mut backend = crate::CpalMicBackend::with_format(None, (48_000, 1));
        let stop = backend.stop_flag.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        backend.handle = Some(thread::spawn(move || {
            ready_tx.send(()).expect("test readiness receiver");
            wait_until_stopped(&stop);
            exited_tx.send(()).expect("test exit receiver");
        }));
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("owner ready");
        if drop_backend {
            drop(backend);
        } else {
            backend.stop();
        }
        exited_rx
            .try_recv()
            .expect("stop/drop must join the owner before returning");
    }
}
