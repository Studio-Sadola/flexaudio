import { createRequire } from 'node:module';
import { test as nodeTest } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
let native;
const addonPath = process.env.FLEXAUDIO_REPRO_ADDON || process.env.FLEXAUDIO_TEST_ADDON || fileURLToPath(new URL('./flexaudio.node', import.meta.url));
try { native = createRequire(import.meta.url)(addonPath); }
catch (error) { if (error.code !== 'MODULE_NOT_FOUND' || process.env.FLEXAUDIO_RUN_REPRO === '1') throw error; }
function test(name, options, fn) {
  if (typeof options === 'function') { fn = options; options = {}; }
  return nodeTest(name, native ? options : { skip: options.skip || 'native build blocked: libspa E0425 SPA_ID_INVALID' }, fn);
}
const runRed = process.env.FLEXAUDIO_RUN_REPRO === '1';
const red = () => ({});
if (runRed) assert.equal(typeof native.__reproP9Bridge, 'function', 'required repro addon fixture is absent');
async function bridge(scenario) {
  const chunks = [];
  const stream = native.__reproP9Bridge(scenario, c => chunks.push(c));
  await stream.stop();
  return chunks;
}
test('repro_p9_orphan_speech', red('C F41'), async () => {
  const chunks = await bridge('orphan');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'speechEnd')),
    'orphan secondary speechEnd discarded without delivery or loss accounting');
});
test('repro_p9_pairing_control', async () => {
  const chunks = await bridge('paired');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'speechEnd')));
});
test('repro_p9_terminator_counters', red('D M13'), async () => {
  const chunks = await bridge('paired');
  const tail = chunks.at(-1);
  assert.equal(tail.frames, 0);
  assert.ok(tail.seq >= chunks[0].seq, `terminator sequence reset: ${chunks[0].seq} -> ${tail.seq}`);
  assert.ok(tail.droppedBefore >= chunks[0].droppedBefore, 'cumulative drops reset');
});
test('repro_p9_bridge_panic', red('C F37; D M11'), async () => {
  const stream = native.__reproP9Bridge('panic', () => {});
  await assert.rejects(stream.stop(), /bridge failure/);
});
test('repro_p9_stop_control', async () => {
  const chunks = [];
  const stream = native.__openMockStream(48000, 1, 440, c => chunks.push(c));
  await new Promise(resolve => setTimeout(resolve, 100));
  await stream.stop();
  assert.ok(chunks.some(c => c.frames > 0));
  assert.equal(chunks.at(-1).frames, 0);
});
test('repro_p9_residual_speech', red('C F41'), async () => {
  const chunks = await bridge('residual');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'speechEnd')),
    'residual secondary speechEnd lost at bridge shutdown');
});
test('repro_p9_thread_exhaustion', red('C F51'), () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    try { native.__reproP9Bridge('exhaust', () => {}); process.exit(2); }
    catch (error) { console.log(error.code); process.exit(0); }
  `, addonPath], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, `binding aborted instead of throwing: signal=${output.signal}\n${output.stderr}`);
  assert.match(output.stdout, /^FLEX_FAILURE$/m);
});
test('repro_p9_throwing_callback_observation', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    let observed = false;
    process.once('uncaughtException', error => { observed = error.message === 'repro callback exception'; });
    const stream = native.__openMockStream(48000, 1, 440, () => {
      if (!observed) throw new Error('repro callback exception');
    });
    setTimeout(async () => { await stream.stop(); console.log('observed=' + observed); process.exit(observed ? 0 : 1); }, 100);
  `, addonPath], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, output.stderr);
  assert.match(output.stdout, /observed=true/);
});

test('throwing callbacks preserve exception identity and allow checked shutdown', () => {
  const addons = new Set([addonPath, process.env.FLEXAUDIO_TEST_ADDON].filter(Boolean));
  for (const path of addons) {
    const output = spawnSync(process.execPath, ['-e', `
      const assert = require('node:assert/strict');
      const native = require(process.argv[1]);
      const thrown = new Error('repro callback exception');
      thrown.code = 'CALLBACK_TEST';
      let observed;
      let continued = false;
      process.once('uncaughtException', error => { observed = error; });
      const stream = native.__openMockStream(48000, 1, 440, chunk => {
        if (!observed) throw thrown;
        if (chunk.frames > 0) continued = true;
      });
      setTimeout(async () => {
        assert.strictEqual(observed, thrown);
        assert.equal(observed.code, 'CALLBACK_TEST');
        await stream.stop();
        assert.equal(continued, true);
        assert.equal(stream.terminalError(), undefined);
        const report = stream.shutdownReport();
        assert.equal(report.primary, null);
        assert.deepEqual(report.cleanupErrors, []);
        console.log('exception retained; stop resolved');
        process.exit(0);
      }, 100);
    `, path], { encoding: 'utf8', timeout: 10000 });
    assert.equal(output.status, 0, `${path}: ${output.stderr}`);
    assert.match(output.stdout, /exception retained; stop resolved/);
  }
});

test('optional lifecycle getters return undefined until a report is retained', async () => {
  const stream = native.__openMockStream(48000, 1, 440, () => {});
  assert.equal(stream.terminalError(), undefined);
  assert.equal(stream.shutdownReport(), undefined);
  await stream.stop();
  assert.equal(stream.terminalError(), undefined);
  const report = stream.shutdownReport();
  assert.equal(report.primary, null);
  assert.deepEqual(report.cleanupErrors, []);
});

test('repro_p9_reaper_failure_control', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    const stream = native.__reproP9Bridge('reaper', () => {});
    setTimeout(async () => { await stream.stop(); console.log('stop resolved despite reaper spawn failure'); process.exit(0); }, 100);
  `, addonPath], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, output.stderr);
  assert.match(output.stdout, /stop resolved despite reaper spawn failure/);
});


test('repro_p9_vad_at_sample_bigint', () => {
  const events = native.__reproP9VadEvents();
  const counts = [9007199254740993n, 9223372036854775808n, 18446744073709551615n];
  let index = 0;
  for (const count of counts) for (const type of ['speechStart', 'speechEnd']) for (const timed of [false, true]) {
    const event = events[index++];
    assert.equal(event.type, type);
    assert.equal(typeof event.atSample, 'bigint');
    assert.equal(event.atSample, count);
    assert.equal(event.atNs, timed ? 1500000000 : undefined);
  }
  assert.equal(index, events.length);
});

test('typed domain exceptions keep codes, native formats, contexts and related failures', () => {
  const codes = {
    invalidArg: 'FLEX_INVALID_ARG', invalidState: 'FLEX_INVALID_STATE',
    deviceNotFound: 'FLEX_DEVICE_NOT_FOUND', permissionDenied: 'FLEX_PERMISSION_DENIED',
    unsupportedOsVersion: 'FLEX_UNSUPPORTED_OS_VERSION', deviceLost: 'FLEX_DEVICE_LOST',
    backend: 'FLEX_FAILURE', unsupportedFormat: 'FLEX_UNSUPPORTED_FORMAT',
    nativeFormatChanged: 'FLEX_NATIVE_FORMAT_CHANGED', unsupported: 'FLEX_UNSUPPORTED',
    ambiguousDeviceName: 'FLEX_AMBIGUOUS_DEVICE_NAME',
  };
  for (const [kind, code] of Object.entries(codes)) {
    assert.throws(() => native.__reproP9Error(kind), error => {
      assert.ok(error instanceof Error);
      assert.equal(error.code, code);
      assert.equal(error.audioError.kind, kind);
      assert.deepEqual(error.audioError.contexts, []);
      assert.deepEqual(error.audioError.secondary, []);
      if (kind === 'nativeFormatChanged') {
        assert.deepEqual(error.audioError.advertised, { sampleRate: 48000, channels: 2 });
        assert.deepEqual(error.audioError.actual, { sampleRate: 44100, channels: 1 });
      }
      if (kind === 'permissionDenied') assert.equal(error.audioError.permission, 'microphone');
      assert.ok(!error.message.includes('private detail'));
      return true;
    });
  }
  assert.throws(() => native.__reproP9Error('wrapped'), error => {
    assert.equal(error.code, 'FLEX_PERMISSION_DENIED');
    assert.equal(error.audioError.kind, 'permissionDenied');
    assert.equal(error.audioError.permission, 'microphone');
    assert.deepEqual(error.audioError.contexts, [
      { operation: 'stop', lane: null, nativeStatus: null },
      { operation: 'start', lane: 'microphone', nativeStatus: { type: 'hresult', call: 'fixture_call', bits: 2147942405 } },
    ]);
    assert.equal(error.audioError.secondary[0].kind, 'backend');
    assert.deepEqual(error.audioError.secondary[0].contexts, [
      { operation: 'join', lane: 'systemAudio', nativeStatus: { type: 'osStatus', call: 'fixture_cleanup', value: -50 } },
    ]);
    assert.equal(error.audioError.secondary[0].secondary[0].kind, 'deviceLost');
    assert.ok(!error.message.includes('private detail'));
    assert.ok(!error.message.includes('fixture_call'));
    return true;
  });
});

test('new stream and device events cross N-API with exact payloads', () => {
  const { streams, devices } = native.__reproP9Payloads();
  assert.deepEqual(streams.slice(0, 3).map(event => event.type), ['recoverableError', 'shutdownError', 'terminalError']);
  for (const event of streams.slice(0, 3)) assert.equal(event.error.kind, 'deviceLost');
  const losses = streams.filter(event => event.type === 'audioLoss');
  assert.equal(losses.length, 4);
  assert.deepEqual(losses[0].loss, { path: { type: 'capture', lane: null }, reason: 'rawOverflow', samples: null, sampleRate: 48000, channels: 2 });
  assert.deepEqual(losses[1].loss.path, { type: 'capture', lane: 'microphone' });
  assert.deepEqual(losses[2].loss.path, { type: 'mixFifo', lane: 'systemAudio' });
  assert.deepEqual(losses[3].loss.path, { type: 'output', tap: 'secondary' });
  for (const event of losses.slice(1)) assert.equal(event.loss.samples, 18446744073709551615n);
  assert.deepEqual(streams.at(-2), { type: 'clipped' });
  assert.deepEqual(streams.at(-1), { type: 'permissionGranted', permission: 'microphone' });
  assert.deepEqual(devices.slice(0, 2), [{ type: 'defaultCleared', sourceKind: 'mic' }, { type: 'defaultCleared', sourceKind: 'system' }]);
  for (const [index, count] of [9007199254740993n, 9223372036854775808n, 18446744073709551615n].entries()) {
    assert.deepEqual(devices[index + 2], { type: 'rescanRequired', droppedEvents: count });
    assert.deepEqual(streams[index + 3], { type: 'chunkDropped', count });
  }
});

test('chunkMs defaults to 20 and rejects non-20 values before acquisition', () => {
  assert.equal(native.__reproP9ChunkMs({ kind: 'mic' }), 20);
  assert.equal(native.__reproP9ChunkMs({ kind: 'mic', chunkMs: 20 }), 20);
  for (const chunkMs of [0, 10, 40, 20.5, -20, NaN, Infinity, 4294967316]) {
    assert.throws(() => native.__reproP9ChunkMs({ kind: 'mic', chunkMs }), error => {
      assert.equal(error.code, 'FLEX_INVALID_ARG');
      assert.equal(error.audioError.kind, 'invalidArg');
      return true;
    });
  }
});

test('bridge cleanup failure is retained across repeated stop without a capture primary', async () => {
  const stream = native.__reproP9Bridge('panic', () => {});
  for (let index = 0; index < 2; index++) {
    await assert.rejects(stream.stop(), error => {
      assert.match(error.message, /bridge failure/);
      assert.equal(error.code, 'FLEX_FAILURE');
      assert.equal(error.audioError.kind, 'backend');
      assert.equal(error.audioError.contexts[0].operation, 'join');
      return true;
    });
  }
  assert.equal(stream.terminalError(), undefined);
  const report = stream.shutdownReport();
  assert.equal(report.primary, null);
  assert.equal(report.cleanupErrors.length, 1);
  assert.equal(report.cleanupErrors[0].kind, 'backend');
});

test('asynchronous failures reject with the same typed audio error tree', async () => {
  await assert.rejects(native.__reproP9AsyncError('wrapped'), error => {
    assert.equal(error.code, 'FLEX_PERMISSION_DENIED');
    assert.equal(error.audioError.kind, 'permissionDenied');
    assert.deepEqual(error.audioError.contexts.map(context => context.operation), ['stop', 'start']);
    assert.equal(error.audioError.secondary[0].secondary[0].kind, 'deviceLost');
    return true;
  });
});

for (const processing of [true, false]) {
  for (const coreCleanup of [false, true]) {
    const scenario = `whisper-${processing ? 'process' : 'flush'}${coreCleanup ? '-cleanup' : ''}`;
    test(`${scenario}: stop and report retain addon and core causes across all waiters`, async () => {
      const stream = native.__reproP9Bridge(scenario, () => {});
      const whisperCode = processing ? 'InvalidPcm' : 'Conversion';
      let retainedError;
      const checkError = error => {
        assert.equal(error.code, !processing && coreCleanup ? 'FLEX_DEVICE_LOST' : whisperCode);
        const report = stream.shutdownReport();
        assert.ok(report);
        if (processing) {
          assert.equal(report.primary.whisperCode, whisperCode);
          assert.equal(report.primary.contexts[0].operation, 'normalize');
          assert.equal(report.cleanupErrors.length, coreCleanup ? 1 : 0);
          if (coreCleanup) assert.equal(report.cleanupErrors[0].kind, 'deviceLost');
        } else {
          assert.equal(report.primary, null);
          assert.equal(report.cleanupErrors.length, coreCleanup ? 2 : 1);
          assert.equal(report.cleanupErrors.at(-1).whisperCode, whisperCode);
          assert.equal(report.cleanupErrors.at(-1).contexts[0].operation, 'flush');
          assert.equal(stream.terminalError(), undefined, 'flush-only failure is cleanup');
        }
        const causes = report.primary ? [report.primary, ...report.cleanupErrors] : report.cleanupErrors;
        const [root, ...secondary] = causes;
        assert.deepEqual(error.audioError, { ...root, secondary: [...root.secondary, ...secondary] });
        if (retainedError) assert.deepEqual(error.audioError, retainedError);
        retainedError = error.audioError;
        return true;
      };
      // Concurrent stop waiters and a call made after completion share one outcome.
      await Promise.all([assert.rejects(stream.stop(), checkError), assert.rejects(stream.stop(), checkError)]);
      const report = stream.shutdownReport();
      await assert.rejects(stream.stop(), checkError);
      assert.deepEqual(stream.shutdownReport(), report);
    });
  }
}

test('core capture primary, core cleanup and final Whisper failure are all retained', async () => {
  const stream = native.__reproP9Bridge('whisper-flush-primary', () => {});
  for (let index = 0; index < 2; index++) {
    await assert.rejects(stream.stop(), error => {
      assert.equal(error.code, 'FLEX_UNSUPPORTED_FORMAT');
      const report = stream.shutdownReport();
      assert.equal(report.primary.kind, 'unsupportedFormat');
      assert.deepEqual(report.cleanupErrors.map(cause => cause.kind), ['deviceLost', 'backend']);
      assert.equal(report.cleanupErrors[1].whisperCode, 'Conversion');
      assert.deepEqual(error.audioError.secondary, report.cleanupErrors);
      return true;
    });
  }
});
