#!/usr/bin/env python3
"""Compile live adapter code against silent fixtures; never modify production code.

CPAL/PipeWire use concrete native types with no injectable host. This runner copies
the indicated *current source*, unchanged, into temporary Rust test translation
units. Only the host/stream objects are fakes; core types and RawSink are real.
Temporary sources/binaries are deleted on exit. Rust test arguments are forwarded.
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
MIC = ROOT / "crates/flexaudio-mic/src/lib.rs"
LINUX = ROOT / "crates/flexaudio-os-linux/src/lib.rs"


def between(source, first, last):
    return source[source.index(first):source.index(last, source.index(first))]


def clean(source):
    return "\n".join(line for line in source.splitlines()
                     if not line.startswith("//!") and not line.startswith("#!"))


def mic_source():
    source = MIC.read_text().split("#[cfg(test)]\nmod tests {")[0]
    # Leave the adapter's functions/closures intact. Replace platform modules,
    # whose consent behavior is already covered by the repository's own tests.
    source = source.replace(between(source, "mod capture_owner;", "/// Fallback format"), "")
    config = clean((MIC.parent / "input_config.rs").read_text().split("#[cfg(test)]")[0])
    return (HERE / "mic_fixture.rs").read_text().replace("// LIVE_INPUT_CONFIG", config).replace(
        "// LIVE_ADAPTER", clean(source))


def linux_source():
    source = LINUX.read_text()
    pieces = {
        "// LIVE_PAIRING": between(source, "fn pair_ports(", "/// Resolve a node's app PID"),
        "// LIVE_LINKING": between(source, "    fn try_link(", "    // registry global / global_remove listeners."),
        "// LIVE_CAPTURE": between(source, "fn add_capture_listener(", "/// Build the byte representation"),
        "// LIVE_LIST_DEVICES": between(source, "pub fn list_devices()", "/// PipeWire registry enumeration implementation"),
        "// LIVE_ENQUEUE": between(source, "fn enqueue_event(", "#[cfg(test)]\nmod tests"),
        "// LIVE_JSON_NAME": between(source, "fn extract_json_name(", "// ============================================================================"),
        "// LIVE_READINESS": between(source, "fn run_pw_loop(", "/// PipeWire setup"),
        "// LIVE_PROCESS_START": between(source[source.index("impl CaptureBackend for PwProcessBackend"):], "    fn start(&mut self, sink: RawSink)", "    fn stop(&mut self)"),
        "// LIVE_SYSTEM_STOP": between(source, "    fn stop(&mut self)", "}\n\nimpl Drop for PwSystemBackend"),
        "// LIVE_POLL": between(source[source.index("impl PwDeviceWatcher {"):], "    pub fn poll_event(", "    /// Stop watching"),
        "// LIVE_DEADLINE": between(source, "    let deadline = std::time::Instant::now();", "    // Build DeviceInfo values"),
        "// LIVE_DEVICE_INFO": between(source, "    for n in &state.nodes {", "    Ok(out)\n}"),
        "// LIVE_REMOVE_CALLBACK": between(source, ".global_remove(move |id| {", "    Ok((\n        main_loop,\n        ProcessKeep"),
    }
    pieces["// LIVE_REMOVE_CALLBACK"] = pieces["// LIVE_REMOVE_CALLBACK"].removeprefix(".global_remove(").split("        .register();")[0].rstrip()[:-1]
    # Callback expressions are kept byte-for-byte, including catch_unwind.
    watch = source[source.index("fn setup_watch("):]
    for marker, section, first, last in [
        ("// LIVE_DEFAULT_CALLBACK", watch, ".property(move |_subject, key, _type, value| {", ".ok();"),
        ("// LIVE_SYNC_CALLBACK", source[source.index("fn enumerate_pw("):], ".done(move |id, seq| {", "        .register();"),
    ]:
        expr = between(section, first, last)
        if "property" in first:
            # Include the .ok() and closure's closing braces, excluding the builder.
            expr += ".ok();\n                                0\n                            })"
            expr = expr.removeprefix(".property(")[:-1]
        else:
            expr = expr.removeprefix(".done(").rstrip()[:-1]
        pieces[marker] = expr
    result = (HERE / "linux_fixture.rs").read_text()
    for marker, code in pieces.items():
        result = result.replace(marker, code)
    return result


def main():
    env = dict(os.environ, RUSTUP_HOME="/home/tubome/.rustup", CARGO_HOME="/home/tubome/.cargo")
    package = sys.argv[1]
    packages = ["-p", "flexaudio-core"]
    deps = ROOT / "target/debug/deps"
    # cargo build's top-level rlibs identify the selected dependency versions.
    core = ROOT / "target/debug/libflexaudio_core.rlib"
    if not core.exists():
        subprocess.run(["cargo", "build", *packages], cwd=ROOT, env=env, check=True)
    source = {"p5": mic_source, "p7": linux_source}[package]()
    with tempfile.TemporaryDirectory(prefix="fa-repro-") as folder:
        rust = Path(folder) / "fixture.rs"
        binary = Path(folder) / "fixture"
        rust.write_text(source)
        subprocess.run(["rustc", "--edition=2021", "--test", str(rust), "-o", str(binary),
                        "-L", f"dependency={deps}", "--extern", f"flexaudio_core={core}"], env=env, check=True)
        return subprocess.run([str(binary), *sys.argv[2:]], env=env).returncode


if __name__ == "__main__":
    sys.exit(main())
