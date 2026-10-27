import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import assert from 'node:assert/strict';

let native;
const addonPath = process.env.FLEXAUDIO_REPRO_ADDON || process.env.FLEXAUDIO_TEST_ADDON || fileURLToPath(new URL('./flexaudio.node', import.meta.url));
try { native = createRequire(import.meta.url)(addonPath); }
catch (error) {
  if (error.code !== 'MODULE_NOT_FOUND' || process.env.FLEXAUDIO_RUN_REPRO === '1') throw error;
}

for (const stage of ['create', 'properties', 'throw']) {
  test(`failed FLAC finalize preserves its original typed cause when ${stage} conversion fails`, {
    skip: !native ? 'test addon is not built' : process.platform !== 'linux' && 'the failing encoder fixture requires Linux /proc/self/fd pipes',
  }, () => {
    native.__failNextErrorConversion(stage);
    let failures;
    try {
      assert.throws(() => native.__dropFailingEncoder(true), error => {
        assert.equal(error.code, 'GenericFailure');
        assert.match(error.message, /injected error conversion failure/);
        return true;
      });
    } finally {
      failures = native.__takeErrorConversionFailures();
    }
    assert.equal(failures.length, 1, 'the original native failure reaches the receiver once');
    assert.equal(failures[0].addonKind, 'flacIo');
    assert.match(failures[0].message, /FLAC output I\/O failed/);
    assert.match(failures[0].addonMessage, /seek/i);
    assert.deepEqual(failures[0].secondary, [], 'conversion status is not a second root cause');
    assert.ok(failures[0].diagnosticDetails.some(detail => detail.includes('GenericFailure')));
  });
}
