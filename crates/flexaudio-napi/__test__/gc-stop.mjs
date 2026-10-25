// Run in a child: node:test treats uncaughtException as a test failure even with a listener.
// A forgotten stop()/finalize() must report GC-observed failures as a process warning
// (never an uncaught exception) and the process must keep running.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { setTimeout as delay } from 'node:timers/promises';

const WARNING_NAME = 'FlexaudioShutdownWarning';
const WARNING_CODE = 'FLEXAUDIO_GC_SHUTDOWN';

function assertStreamWarning(warning) {
  assert.ok(warning instanceof Error, 'the warning is an Error object');
  assert.match(warning.message, /stop\(\)/);
  // The same typed fields a rejected stop() carries.
  assert.equal(warning.cause.code, 'FLEX_DEVICE_LOST');
  assert.equal(warning.cause.audioError.kind, 'deviceLost');
  assert.equal(warning.audioError.kind, 'deviceLost');
  // Every retained failure stays in the report: the backend stop failure and the VAD flush.
  const report = warning.shutdownReport;
  assert.equal(report.primary, null);
  assert.deepEqual(report.cleanupErrors.map(error => error.kind), ['deviceLost', 'backend']);
  assert.equal(report.cleanupErrors[1].vadCode != null, true, 'the attached VAD failure keeps its typed code');
  assert.equal(warning.audioError.secondary.length, 1, 'the warning error keeps the related VAD failure');
}

function assertEncoderWarning(warning) {
  assert.ok(warning instanceof Error, 'the warning is an Error object');
  assert.match(warning.message, /finalize\(\)/);
  // The same typed error finalize() throws.
  assert.ok(warning.cause instanceof Error);
  assert.equal(warning.cause.code, 'GenericFailure');
  assert.match(warning.cause.message, /seek/i);
  assert.equal(warning.cause.audioError.addonKind, 'flacIo');
  assert.equal(warning.audioError.addonKind, 'flacIo');
}

async function checkCleanup() {
  const native = createRequire(import.meta.url)(process.env.FLEXAUDIO_REPRO_ADDON);
  const mode = process.argv[2];
  // emitWarning itself throws: delivery must still stay non-fatal.
  const throwing = mode === 'implicit-throwing';
  const explicit = mode === 'explicit';
  const encoder = mode.startsWith('encoder');
  const finalized = mode === 'encoder-explicit';
  const quiet = explicit || finalized;
  const uncaught = [];
  const warnings = [];
  const onException = error => uncaught.push(error);
  const onWarning = warning => {
    if (warning.name === WARNING_NAME) warnings.push(warning);
  };
  process.on('uncaughtException', onException);
  process.on('warning', onWarning);
  const emitWarning = process.emitWarning;
  if (throwing) {
    process.emitWarning = warning => {
      onWarning(warning);
      throw new Error('emitWarning failed');
    };
  }
  try {
    let stream = encoder ? null : native.__bindingContractBridge('attachment-flush-cleanup', () => {});
    if (encoder) {
      if (finalized) assert.throws(() => native.__dropFailingEncoder(true), /seek/i);
      else native.__dropFailingEncoder(false);
    }
    if (explicit) {
      await assert.rejects(stream.stop(), error => error.audioError.kind === 'deviceLost');
    }
    stream = null;
    const deadline = performance.now() + (quiet ? 200 : 5000);
    do {
      global.gc();
      await delay(10);
    } while (performance.now() < deadline && (quiet || warnings.length === 0));
    await delay(50);
    assert.deepEqual(uncaught, [], 'a GC-observed failure must never become an uncaught exception');
    assert.equal(warnings.length, quiet ? 0 : 1);
    if (!quiet) {
      const [warning] = warnings;
      assert.equal(warning.name, WARNING_NAME);
      assert.equal(warning.code, WARNING_CODE);
      if (encoder) assertEncoderWarning(warning);
      else assertStreamWarning(warning);
    }
    // The process keeps running after the warning.
    assert.equal(await delay(20, 'alive'), 'alive');
  } finally {
    process.removeListener('uncaughtException', onException);
    process.removeListener('warning', onWarning);
    process.emitWarning = emitWarning;
  }
}

// Assertion failures reject this Promise rather than entering the exception collector.
await checkCleanup().then(
  () => console.log('gc-stop: ok'),
  error => {
    console.error(error);
    process.exitCode = 1;
  },
);
