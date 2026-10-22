import { createRequire } from 'node:module';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';

const addon = process.env.FLEXAUDIO_REPRO_ADDON || process.env.FLEXAUDIO_TEST_ADDON ||
  fileURLToPath(new URL('./flexaudio.node', import.meta.url));
let native;
try { native = createRequire(import.meta.url)(addon); }
catch (error) {
  if (error.code !== 'MODULE_NOT_FOUND' || process.env.FLEXAUDIO_RUN_REPRO === '1') throw error;
}

test('facade stop failure rejects the Node caller and remains in the shutdown report',
  { skip: !native && 'test addon is not built' }, async () => {
    const stream = native.__bindingContractBridge('attachment-flush-cleanup', () => {});
    const rejected = error => {
      assert.equal(error.code, 'FLEX_DEVICE_LOST');
      assert.equal(error.audioError.kind, 'deviceLost');
      const report = stream.shutdownReport();
      assert.equal(report.primary, null);
      assert.equal(report.cleanupErrors[0].kind, 'deviceLost');
      assert.equal(report.cleanupErrors[0].contexts[0].operation, 'stop');
      return true;
    };
    await assert.rejects(stream.stop(), rejected);
    const report = stream.shutdownReport();
    await assert.rejects(stream.stop(), rejected);
    assert.deepEqual(stream.shutdownReport(), report);
  });
