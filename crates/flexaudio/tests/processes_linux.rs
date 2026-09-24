//! Linux real-device integration test for `processes()` (runs only where PipeWire is available).
//!
//! Spawns a child process that plays a silent WAV with `pw-play` and checks that its PID shows
//! up in `processes()` (i.e. that it is visible as a candidate that can be passed as the
//! `target_pid` of per-process capture).
//! Where PipeWire is unreachable or `pw-play` is missing, it prints the reason and succeeds
//! without doing anything.
//!
//! Example run (non-interactive SSH needs `XDG_RUNTIME_DIR` to find the PipeWire socket):
//! ```text
//! XDG_RUNTIME_DIR=/run/user/$(id -u) cargo test -p flexaudio --test processes_linux -- --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Always cleans up the child process and the temporary directory, even if the test panics.
struct PlayerGuard {
    child: Child,
    dir: PathBuf,
}

impl Drop for PlayerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Writes a 48 kHz / stereo / 16-bit silent WAV.
fn write_silent_wav(path: &Path, seconds: u32) -> std::io::Result<()> {
    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    const BITS: u16 = 16;
    let block_align = CHANNELS * (BITS / 8);
    let data_len = RATE * seconds * u32::from(block_align);
    let mut file = fs::File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&CHANNELS.to_le_bytes())?;
    file.write_all(&RATE.to_le_bytes())?;
    file.write_all(&(RATE * u32::from(block_align)).to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&BITS.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    file.write_all(&vec![0u8; data_len as usize])?;
    Ok(())
}

#[test]
fn lists_a_pipewire_playback_process_by_pid() {
    if let Err(e) = flexaudio::processes() {
        eprintln!("SKIP: PipeWire is not reachable from this test process ({e})");
        return;
    }
    if Command::new("pw-play")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("SKIP: `pw-play` is not installed");
        return;
    }

    let dir = std::env::temp_dir().join(format!("flexaudio-processes-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    let wav = dir.join("silence.wav");
    write_silent_wav(&wav, 10).expect("write silent wav");

    let child = Command::new("pw-play")
        .arg(&wav)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pw-play");
    let player = PlayerGuard { child, dir };
    let player_pid = player.child.id();

    // Wait briefly for the playback stream to appear in the registry (up to 5 seconds).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = None;
    while Instant::now() < deadline {
        if let Ok(list) = flexaudio::processes() {
            assert!(
                list.iter().all(|p| p.pid != std::process::id()),
                "the calling process must not be listed"
            );
            if let Some(p) = list.into_iter().find(|p| p.pid == player_pid) {
                found = Some(p);
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    let p = found.unwrap_or_else(|| {
        panic!("pw-play (pid {player_pid}) should be listed by processes() within 5 s")
    });
    eprintln!("found: {p:?}");
    assert!(!p.name.trim().is_empty());
    assert!(
        p.executable.is_some(),
        "the executable name comes from /proc/<pid>/exe"
    );
    assert!(
        p.is_output_active.is_some(),
        "PipeWire reports the node state, so the activity flag is known"
    );
    assert_eq!(p.bundle_id, None, "bundle ids exist only on macOS");
    drop(player);
}
