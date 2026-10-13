//! Native loading failures are simulated without loading an addon or opening devices.
#[test]
fn loader_retains_every_attempted_candidate_and_cause() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("index.js");
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(
            r#"
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const attempts = [];
const causes = [];
const sandbox = {
  process: { platform: 'darwin', arch: 'arm64' }, __dirname: '/fixture', module: { exports: {} },
  require(candidate) {
    if (candidate === 'fs') return { existsSync: () => false, readFileSync: fs.readFileSync };
    if (candidate === 'path') return require('node:path');
    attempts.push(candidate);
    const cause = new Error('injected native load failure ' + attempts.length);
    causes.push(cause);
    throw cause;
  }
};
let failure;
try { vm.runInNewContext(fs.readFileSync(process.argv[1], 'utf8'), sandbox); }
catch (error) { failure = error; }
assert.equal(attempts.length, 2);
assert.equal(failure.attempts.length, 2);
assert.equal(failure.errors.length, 2);
for (let i = 0; i < attempts.length; i++) {
  assert.equal(failure.attempts[i].candidate, attempts[i]);
  assert.equal(failure.attempts[i].error, causes[i]);
  assert.equal(failure.errors[i], causes[i]);
}
"#,
        )
        .arg(source)
        .output()
        .expect("Node is required for offline loader tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
