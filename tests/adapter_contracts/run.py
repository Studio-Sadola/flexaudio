#!/usr/bin/env python3
"""Compile live adapter code against silent fixtures; never modify production code.

CPAL/PipeWire use concrete native types with no injectable host. This runner copies
the indicated *current source*, unchanged, into temporary Rust test translation
units. Only the host/stream objects are fakes; core types and RawSink are real.
Temporary sources/binaries are deleted on exit. Rust test arguments are forwarded.
"""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
MIC = ROOT / "crates/flexaudio-mic/src/lib.rs"
LINUX = ROOT / "crates/flexaudio-os-linux/src/lib.rs"
FACADE = ROOT / "crates/flexaudio/src/lib.rs"


def marker_index(source, marker, start=0):
    try:
        return source.index(marker, start)
    except ValueError as error:
        raise ValueError(f"adapter source marker missing: {marker!r}") from error


def between(source, first, last):
    start = marker_index(source, first)
    end = marker_index(source, last, start + len(first))
    return source[start:end]


def splice(source, marker, code):
    if marker not in source:
        raise ValueError(f"fixture marker missing: {marker}")
    return source.replace(marker, code)


def callback_expression(source, first, last):
    expression = between(source, first, last).rstrip()
    if not expression.endswith(")"):
        raise ValueError(f"adapter callback closing marker missing before: {last!r}")
    prefix = first.partition("(")[0] + "("
    return expression[len(prefix):-1]


def clean(source):
    return "\n".join(line for line in source.splitlines()
                     if not line.startswith("//!") and not line.startswith("#!"))


def mic_source():
    source = MIC.read_text()
    source = source[:marker_index(source, "#[cfg(test)]\nmod tests {")]
    # Leave the adapter's functions/closures intact. Replace platform modules,
    # whose consent behavior is already covered by the repository's own tests.
    source = source.replace(between(source, "mod capture_owner;", "/// Fallback format"), "")
    # Native backend injection has its own production tests. Keep its original
    # gate, but exclude that module from this separate fixture translation unit.
    source = splice(source, "pub mod test_support;",
                    "#[cfg(not(adapter_contract_fixture))]\npub mod test_support;")
    config_source = (MIC.parent / "input_config.rs").read_text()
    config = clean(config_source[:marker_index(config_source, "#[cfg(test)]")] + between(
        config_source, "/// Materialize a query failure", "#[cfg(test)]\nmod diagnostic_tests"))
    devices = between(FACADE.read_text(), "pub fn devices()", "/// Start a [`DeviceWatcher`]")
    linux_devices = between(LINUX.read_text(), "pub fn list_devices()", "/// PipeWire registry enumeration implementation")
    result = (HERE / "mic_fixture.rs").read_text()
    for marker, code in {
        "// LIVE_INPUT_CONFIG": config, "// LIVE_ADAPTER": clean(source),
        "// LIVE_FACADE_DEVICES": devices,
        "// LIVE_PIPEWIRE_LIST_DEVICES": linux_devices,
    }.items():
        result = splice(result, marker, code)
    return result


def linux_source():
    source = LINUX.read_text()
    pieces = {
        "// LIVE_WATCHER_STATE": "mod watcher_state {\n" + clean((LINUX.parent / "watcher_state.rs").read_text()) + "\n}",
        "// LIVE_USER_DATA": between(source, "struct UserData {", "/// Register `param_changed`"),
        "// LIVE_PID_PROCESS": between(source, "    fn process(target_pid:", "    /// Use the same decision"),
        "// LIVE_PAIRING": between(source, "fn pair_ports(", "/// Resolve a node's app PID"),
        "// LIVE_LINKING": between(source, "    fn try_link(", "    // registry global / global_remove listeners."),
        "// LIVE_CAPTURE": between(source, "fn add_capture_listener(", "/// Build the byte representation"),
        "// LIVE_LIST_DEVICES": between(source, "pub fn list_devices()", "/// PipeWire registry enumeration implementation"),
        "// LIVE_ENQUEUE": between(source, "fn enqueue_event(", "#[cfg(test)]\nmod tests"),
        "// LIVE_JSON_NAME": between(source, "fn extract_json_name(", "// ============================================================================"),
        "// LIVE_READINESS": between(source, "fn run_pw_loop(", "/// PipeWire setup"),
        "// LIVE_PROCESS_START": between(source[marker_index(source, "impl CaptureBackend for PwProcessBackend"):], "    fn start(&mut self, sink: RawSink)", "    fn stop(&mut self)"),
        # stop and stop_checked call retain_shutdown_outcome, which lives in a later impl.
        # Keep the system helper that those extracted methods call; stop before the process copy.
        "// LIVE_SYSTEM_STOP": between(source, "    fn stop(&mut self)", "}\n\nimpl Drop for PwSystemBackend")
            + between(
                source,
                "    fn retain_shutdown_outcome(&mut self) {",
                "}\n\nimpl PwProcessBackend",
            ),
        "// LIVE_POLL": between(source[marker_index(source, "impl PwDeviceWatcher {"):], "    pub fn poll_event(", "    /// Stop watching"),
        "// LIVE_DEADLINE": between(source, "    let deadline = std::time::Instant::now();", "    // Build DeviceInfo values"),
        "// LIVE_REMOVE_CALLBACK": callback_expression(source, ".global_remove(move |id| {", "        .register();"),
    }
    # Callback expressions are kept byte-for-byte, including catch_unwind.
    watch = source[marker_index(source, "fn setup_watch("):]
    for marker, section, first, last in [
        ("// LIVE_DEFAULT_CALLBACK", watch, ".property(move |_subject, key, _type, value| {", ".register();"),
        ("// LIVE_SYNC_CALLBACK", source[marker_index(source, "fn enumerate_pw("):], ".done(move |id, seq| {", "        .register();"),
    ]:
        pieces[marker] = callback_expression(section, first, last)
    result = (HERE / "linux_fixture.rs").read_text()
    for marker, code in pieces.items():
        result = splice(result, marker, code)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", choices=["mic", "linux"])
    args, test_args = parser.parse_known_args()
    env = os.environ.copy()
    # Includes resolve against this checkout even when invoked from another directory.
    env["ADAPTER_ROOT"] = str(ROOT)
    package = args.package
    packages = ["-p", "flexaudio-core"]
    deps = ROOT / "target/debug/deps"
    # cargo build's top-level rlibs identify the selected dependency versions.
    core = ROOT / "target/debug/libflexaudio_core.rlib"
    # Cargo checks freshness; a cached rlib may belong to an older checkout.
    subprocess.run(["cargo", "build", "--locked", "--offline", *packages], cwd=ROOT, env=env, check=True)
    source = {"mic": mic_source, "linux": linux_source}[package]()
    with tempfile.TemporaryDirectory(prefix="flexaudio-adapter-") as folder:
        rust = Path(folder) / "fixture.rs"
        binary = Path(folder) / "fixture"
        rust.write_text(source)
        subprocess.run(["rustc", "--edition=2021", "--test", "--cfg", "adapter_contract_fixture", str(rust), "-o", str(binary),
                        "-L", f"dependency={deps}", "--extern", f"flexaudio_core={core}"], env=env, check=True)
        return subprocess.run([str(binary), *test_args], env=env).returncode


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ValueError as error:
        sys.exit(f"adapter contract harness: {error}")
