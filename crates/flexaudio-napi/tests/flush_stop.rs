//! Exercise the real Node command queue and promise settlement through the cdylib.
#[cfg(target_os = "linux")]
#[test]
fn flush_requested_immediately_before_stop_always_settles() {
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let directory =
        std::env::temp_dir().join(format!("flexaudio-flush-stop-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let addon = directory.join("flexaudio.node");
    std::fs::copy(
        deps.parent()
            .unwrap()
            .join("examples/libcapture_test_addon.so"),
        &addon,
    )
    .unwrap();
    let result = std::process::Command::new("node")
        .args([
            "-e",
            r#"
const assert = require('node:assert/strict');
const native = require(process.argv[1]);
(async () => {
  for (let i = 0; i < 16; i++) {
    const stream = native.__openMockStream(48000, 2, 0, () => {},
      undefined, undefined, undefined, undefined, undefined,
      { tap: 'primary', params: {} });
    const p = stream.flushWhisperVad();
    const settled = p.then(() => 'resolved', error => {
      assert.equal(typeof error.code, 'string');
      return 'rejected';
    });
    await stream.stop();
    let timer;
    try {
      await Promise.race([settled, new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error('flush remained pending after stop')), 1000);
      })]);
    } finally { clearTimeout(timer); }
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
"#,
        ])
        .arg(&addon)
        .output();
    std::fs::remove_dir_all(&directory).unwrap();
    let output = result.unwrap();
    assert!(
        output.status.success(),
        "Node flush/stop regression: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
