//! Check that the checked-in header is exactly the repository cbindgen projection.
#[test]
fn generated_header_matches_cbindgen() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::env::temp_dir().join(format!("flexaudio-header-{}.h", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(output.clone());
    let status = std::process::Command::new("cbindgen")
        .args([
            "--config",
            "cbindgen.toml",
            "--crate",
            "flexaudio-ffi",
            "--output",
        ])
        .arg(&output)
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("cbindgen must be installed to verify the generated C ABI");
    assert!(
        status.status.success(),
        "cbindgen generation failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("include/flexaudio.h")).unwrap(),
        std::fs::read(output).unwrap(),
        "regenerate include/flexaudio.h using cbindgen.toml"
    );
}
