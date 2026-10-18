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
  return nodeTest(name, native ? options : { skip: options.skip || 'test addon is not built' }, fn);
}
const requireFixtures = process.env.FLEXAUDIO_RUN_REPRO === '1';
if (requireFixtures) assert.equal(typeof native.__bindingContractBridge, 'function', 'required test addon fixture is absent');
async function bridge(scenario) {
  const chunks = [];
  const stream = native.__bindingContractBridge(scenario, c => chunks.push(c));
  await stream.stop();
  return chunks;
}
test('binding_contract_orphan_speech', async () => {
  const chunks = await bridge('orphan');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'segment')),
    'orphan secondary segment discarded without delivery or loss accounting');
});
test('binding_contract_pairing_control', async () => {
  const chunks = await bridge('paired');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'segment')));
});
test('binding_contract_terminator_counters', async () => {
  const chunks = await bridge('paired');
  const tail = chunks.at(-1);
  assert.equal(tail.frames, 0);
  assert.ok(tail.seq >= chunks[0].seq, `terminator sequence reset: ${chunks[0].seq} -> ${tail.seq}`);
  assert.ok(tail.droppedBefore >= chunks[0].droppedBefore, 'cumulative drops reset');
});
test('binding_contract_bridge_panic', async () => {
  const stream = native.__bindingContractBridge('panic', () => {});
  await assert.rejects(stream.stop(), /bridge failure/);
});
test('binding_contract_stop_control', async t => {
  const chunks = [];
  const stream = native.__openMockStream(48000, 1, 440, c => chunks.push(c));
  t.after(async () => { await stream.stop(); });
  await new Promise(resolve => setTimeout(resolve, 100));
  await stream.stop();
  assert.ok(chunks.some(c => c.frames > 0));
  assert.equal(chunks.at(-1).frames, 0);
});
test('binding_contract_residual_speech', async () => {
  const chunks = await bridge('residual');
  assert.ok(chunks.some(c => c.secondary?.vadEvents?.some(e => e.type === 'segment')),
    'residual secondary segment lost at bridge shutdown');
});
test('binding_contract_thread_exhaustion', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    try { native.__bindingContractBridge('exhaust', () => {}); process.exit(2); }
    catch (error) { console.log(error.code); process.exit(0); }
  `, addonPath], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, `binding aborted instead of throwing: signal=${output.signal}\n${output.stderr}`);
  assert.match(output.stdout, /^FLEX_FAILURE$/m);
});
test('binding_contract_throwing_callback_observation', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    let observed = false;
    process.once('uncaughtException', error => { observed = error.message === 'repro callback exception'; });
    const stream = native.__openMockStream(48000, 1, 440, () => {
      if (!observed) throw new Error('repro callback exception');
    });
    setTimeout(async () => { await stream.stop(); console.log('observed=' + observed); if (!observed) process.exitCode = 1; }, 100);
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
        try {
          assert.strictEqual(observed, thrown);
          assert.equal(observed.code, 'CALLBACK_TEST');
          await stream.stop();
          assert.equal(continued, true);
          assert.equal(stream.terminalError(), undefined);
          const report = stream.shutdownReport();
          assert.equal(report.primary, null);
          assert.deepEqual(report.cleanupErrors, []);
          console.log('exception retained; stop resolved');
        } finally { await stream.stop(); }
      }, 100);
    `, path], { encoding: 'utf8', timeout: 10000 });
    assert.equal(output.status, 0, `${path}: ${output.stderr}`);
    assert.match(output.stdout, /exception retained; stop resolved/);
  }
});

test('optional lifecycle getters return undefined until a report is retained', async t => {
  const stream = native.__openMockStream(48000, 1, 440, () => {});
  t.after(async () => { await stream.stop(); });
  assert.equal(stream.terminalError(), undefined);
  assert.equal(stream.shutdownReport(), undefined);
  await stream.stop();
  assert.equal(stream.terminalError(), undefined);
  const report = stream.shutdownReport();
  assert.equal(report.primary, null);
  assert.deepEqual(report.cleanupErrors, []);
});

test('binding_contract_reaper_failure_control', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    const stream = native.__bindingContractBridge('reaper', () => {});
    setTimeout(async () => { await stream.stop(); console.log('stop resolved despite reaper spawn failure'); }, 100);
  `, addonPath], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, output.stderr);
  assert.match(output.stdout, /stop resolved despite reaper spawn failure/);
});


test('capture origin bigint preserves the entire unsigned domain', () => {
  const events = native.__bindingContractVadEvents();
  const counts = [9007199254740993n, 9223372036854775808n, 18446744073709551615n];
  assert.equal(events.length, counts.length);
  for (const [index, count] of counts.entries()) {
    assert.deepEqual(events[index], { type: 'epochStart', epoch: 0, seq: 0,
      captureSample: count, ptsNs: 1500000000 });
  }
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
    assert.throws(() => native.__bindingContractError(kind), error => {
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
  assert.throws(() => native.__bindingContractError('wrapped'), error => {
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
  const { streams, devices } = native.__bindingContractPayloads();
  assert.deepEqual(streams.slice(0, 3).map(event => event.type), ['recoverableError', 'shutdownError', 'terminalError']);
  for (const event of streams.slice(0, 3)) assert.equal(event.error.kind, 'deviceLost');
  const losses = streams.filter(event => event.type === 'audioLoss');
  assert.equal(losses.length, 4);
  assert.deepEqual(losses[0].loss, { path: { type: 'capture', lane: null }, reason: 'rawOverflow', samples: null, sampleRate: 48000, channels: 2 });
  assert.deepEqual(losses[1].loss.path, { type: 'capture', lane: 'microphone' });
  assert.deepEqual(losses[2].loss.path, { type: 'mixFifo', lane: 'systemAudio' });
  assert.deepEqual(losses[3].loss.path, { type: 'output', tap: 'secondary' });
  for (const event of losses.slice(1)) assert.equal(event.loss.samples, 18446744073709551615n);
  assert.deepEqual(streams.at(-1), { type: 'clipped' });
  assert.deepEqual(devices.slice(0, 2), [{ type: 'defaultCleared', sourceKind: 'mic' }, { type: 'defaultCleared', sourceKind: 'system' }]);
  for (const [index, count] of [9007199254740993n, 9223372036854775808n, 18446744073709551615n].entries()) {
    assert.deepEqual(devices[index + 2], { type: 'rescanRequired', droppedEvents: count });
    assert.deepEqual(streams[index + 3], { type: 'chunkDropped', count });
  }
});

test('chunkMs defaults to 20 and rejects non-20 values before acquisition', () => {
  assert.equal(native.__bindingContractChunkMs({ kind: 'mic' }), 20);
  assert.equal(native.__bindingContractChunkMs({ kind: 'mic', chunkMs: 20 }), 20);
  for (const chunkMs of [0, 10, 40, 20.5, -20, NaN, Infinity, 4294967316]) {
    assert.throws(() => native.__bindingContractChunkMs({ kind: 'mic', chunkMs }), error => {
      assert.equal(error.code, 'FLEX_INVALID_ARG');
      assert.equal(error.audioError.kind, 'invalidArg');
      assert.match(error.message, /canonical 20 ms frame contract/);
      assert.match(error.message, /set chunkMs to 20/);
      assert.ok(!error.message.includes('chunk_ms'));
      return true;
    });
  }
});

test('bridge cleanup failure is retained across repeated stop without a capture primary', async () => {
  const stream = native.__bindingContractBridge('panic', () => {});
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
  await assert.rejects(native.__bindingContractAsyncError('wrapped'), error => {
    assert.equal(error.code, 'FLEX_PERMISSION_DENIED');
    assert.equal(error.audioError.kind, 'permissionDenied');
    assert.deepEqual(error.audioError.contexts.map(context => context.operation), ['stop', 'start']);
    assert.equal(error.audioError.secondary[0].secondary[0].kind, 'deviceLost');
    return true;
  });
});

for (const processing of [true, false]) {
  for (const coreCleanup of [false, true]) {
    const scenario = `attachment-${processing ? 'process' : 'flush'}${coreCleanup ? '-cleanup' : ''}`;
    test(`${scenario}: stop and report retain addon and core causes across all waiters`, async () => {
      const log = [];
      const stream = native.__bindingContractBridge(scenario,
        chunk => log.push({ kind: 'chunk', events: chunk.vadEvents ?? [] }),
        event => log.push({ kind: 'event', event }));
      const vadCode = 'Inference';
      const checkReport = () => {
        const report = stream.shutdownReport();
        assert.ok(report);
        assert.equal(report.primary, null, 'an attached VAD-only failure is never the capture primary');
        const attachment = report.cleanupErrors.filter(cause => cause.vadCode === vadCode);
        assert.equal(attachment.length, 1, 'the addon cause is retained exactly once');
        assert.equal(attachment[0].contexts[0].operation, processing ? 'normalize' : 'flush');
        assert.equal(report.cleanupErrors.length, coreCleanup ? 2 : 1);
        if (coreCleanup) assert.equal(report.cleanupErrors[0].kind, 'deviceLost');
        assert.equal(stream.terminalError(), undefined);
        return report;
      };
      let retainedError;
      const checkError = error => {
        assert.equal(error.code, 'FLEX_DEVICE_LOST');
        assert.equal(error.message, 'device lost during stop during stop',
          'isolated VAD cleanup must preserve the capture exception message');
        const report = checkReport();
        const [root, ...secondary] = report.cleanupErrors;
        assert.deepEqual(error.audioError, { ...root, secondary: [...root.secondary, ...secondary] });
        if (retainedError) assert.deepEqual(error.audioError, retainedError);
        retainedError = error.audioError;
        return true;
      };
      // Register all stop waiters before testing results, so assertion failures cannot leave intake live.
      const first = stream.stop();
      const second = stream.stop();
      if (coreCleanup) {
        await Promise.all([assert.rejects(first, checkError), assert.rejects(second, checkError)]);
        await assert.rejects(stream.stop(), checkError);
      } else {
        await Promise.all([first, second]);
        log.push({ kind: 'resolved' });
        checkReport();
        const carrier = log.findIndex(entry => entry.kind === 'chunk' && entry.events
          .some(event => event.type === 'epochEnd' && event.reason === 'error'));
        const failure = log.findIndex(entry => entry.kind === 'event' && entry.event.type === 'vadFailed');
        assert.ok(carrier >= 0 && carrier < failure, 'final failure closure must arrive before its notification');
        assert.ok(failure < log.findIndex(entry => entry.kind === 'resolved'), 'vadFailed must arrive before stop resolves');
        assert.equal(log[failure].event.error.vadCode, vadCode);
        assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'vadFailed').length, 1);
        await stream.stop();
      }
      const report = stream.shutdownReport();
      assert.deepEqual(stream.shutdownReport(), report);
    });
  }
}

test('core capture primary, core cleanup and final VAD failure are all retained', async () => {
  const stream = native.__bindingContractBridge('attachment-flush-primary', () => {});
  for (let index = 0; index < 2; index++) {
    await assert.rejects(stream.stop(), error => {
      assert.equal(error.code, 'FLEX_UNSUPPORTED_FORMAT');
      const report = stream.shutdownReport();
      assert.equal(report.primary.kind, 'unsupportedFormat');
      assert.deepEqual(report.cleanupErrors.map(cause => cause.kind), ['deviceLost', 'backend']);
      assert.equal(report.cleanupErrors[1].vadCode, 'Inference');
      assert.deepEqual(error.audioError.secondary, report.cleanupErrors);
      return true;
    });
  }
});

for (const scenario of ['attachment-secondary-orphan', 'attachment-pending-flush', 'attachment-pending-stop']) {
  test(`${scenario}: pending closure precedes one notification and retains its cause`, async () => {
    const log = [];
    const stream = native.__bindingContractBridge(scenario,
      chunk => log.push({ kind: 'chunk', chunk }),
      event => log.push({ kind: 'event', event }));
    await stream.stop();
    log.push({ kind: 'resolved' });
    const carrier = log.findIndex(entry => entry.kind === 'chunk'
      && (entry.chunk.secondary?.vadEvents ?? []).some(event => event.type === 'epochEnd' && event.reason === 'error'));
    const failure = log.findIndex(entry => entry.kind === 'event' && entry.event.type === 'vadFailed');
    assert.ok(carrier >= 0 && carrier < failure, 'the selected-tap error closure must precede the notification');
    assert.ok(failure < log.length - 1, 'the failure must arrive before stop resolves');
    assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'vadFailed').length, 1);
    if (scenario === 'attachment-secondary-orphan') {
      assert.ok(log[carrier].chunk.frames > 0, 'an orphan secondary failure must ride real primary PCM');
      assert.ok(log.filter(entry => entry.kind === 'chunk').every(entry => entry.chunk.frames > 0));
    }
    assert.equal(log[carrier].chunk.secondary.seq, 17n, 'secondary closure keeps the last sequence');
    assert.equal(log[carrier].chunk.secondary.droppedBefore, 5, 'secondary closure keeps the last cumulative drops');
    assert.equal(log[carrier].chunk.secondary.frameIndex, 0n, 'secondary closure keeps the last observed frame index');
    const report = stream.shutdownReport();
    assert.equal(report.primary, null);
    assert.equal(report.cleanupErrors.length, 1);
    assert.equal(report.cleanupErrors[0].vadCode, 'Inference');
  });
}


test('startup drop precedes the first chunk and VAD epoch on the callback queue', async () => {
  const order = [];
  const stream = native.__bindingContractBridge('startup-order', chunk => {
    if (chunk.frames > 0) order.push('chunk');
    for (const event of chunk.vadEvents ?? []) {
      if (event.type === 'epochStart') order.push('epochStart');
    }
  }, event => {
    if (event.type === 'startupSamplesDropped') order.push('startupSamplesDropped');
  });
  await new Promise(resolve => setTimeout(resolve, 50));
  await stream.stop();
  const drop = order.indexOf('startupSamplesDropped');
  const chunk = order.indexOf('chunk');
  const epoch = order.indexOf('epochStart');
  assert.ok(drop >= 0 && chunk >= 0 && epoch >= 0, `missing startup fixture output: ${order}`);
  assert.ok(drop < chunk && drop < epoch, `startup notice arrived after generation data: ${order}`);
});

for (const scenario of ['startup-flush', 'startup-stop-prologue']) {
  test(`${scenario} drains the accepted prefix before finalizing VAD`, async () => {
    const order = [];
    const chunks = [];
    const stream = native.__bindingContractBridge(scenario, chunk => {
      chunks.push(chunk);
      if (chunk.frames > 0) order.push('chunk');
      for (const event of chunk.vadEvents ?? []) order.push(event.type);
    }, event => {
      if (event.type === 'startupSamplesDropped') order.push(event.type);
    });
    await stream.stop();
    assert.equal(chunks.reduce((sum, chunk) => sum + chunk.data.length, 0), 3840,
      'all accepted startup PCM must be delivered before completion');
    const events = chunks.flatMap(chunk => chunk.vadEvents ?? []);
    assert.equal(events.filter(event => event.type === 'epochStart').length, 1,
      'the pending accepted prefix must start its epoch before finalization');
    const ends = events.filter(event => event.type === 'epochEnd');
    assert.equal(ends.length, 1, 'the accepted epoch closes exactly once');
    assert.equal(ends[0].reason, 'finish');
    const notice = order.indexOf('startupSamplesDropped');
    assert.ok(notice >= 0 && notice < order.indexOf('chunk') && notice < order.indexOf('epochStart'),
      `startup notice must precede PCM and epoch events: ${order}`);
    assert.equal(stream.shutdownReport().primary, null);
    assert.deepEqual(stream.shutdownReport().cleanupErrors, []);
  });
}

test('capability snapshots preserve every field, tag and structured query error', () => {
  const snapshots = native.__bindingContractCapabilities();
  const backends = ['cpal', 'pipeWire', 'wasapiLoopback', 'wasapiProcessLoopback',
    'coreAudioProcessTap', 'composite', 'unsupported'];
  assert.equal(snapshots.length, backends.length);
  for (const [index, snapshot] of snapshots.entries()) {
    assert.deepEqual(Object.keys(snapshot).sort(), ['osVersion', 'sources', 'systemExclusions',
      'process', 'deviceWatcher', 'sideEffects'].sort());
    assert.deepEqual(snapshot.osVersion, index === 0 ? null : { major: 14, minor: 4, patch: null, build: 42 });
    assert.deepEqual(Object.keys(snapshot.sources).sort(), ['mic', 'system', 'process', 'mix'].sort());
    for (const source of Object.values(snapshot.sources)) {
      assert.deepEqual(Object.keys(source).sort(), ['availability', 'backend']);
      assert.equal(source.backend, backends[index]);
    }
    const availability = snapshot.sources.mic.availability;
    assert.deepEqual(snapshot.systemExclusions, { availability, backend: backends[index],
      scope: ['exactPid', 'processTree', 'notApplicable'][index % 3],
      maxDistinctPids: index === 0 ? null : 1,
      deviceSelection: ['honored', 'ignored', 'notApplicable'][index % 3] });
    assert.deepEqual(snapshot.process, {
      includeScope: ['exactPid', 'processTree', 'notApplicable'][index % 3],
      excludeScope: ['exactPid', 'processTree', 'notApplicable'][(index + 1) % 3],
    });
    assert.deepEqual(snapshot.deviceWatcher, { availability, mode: index === 0 ? 'noop' : 'native',
      scopes: index === 0 ? [] : ['systemOutputs', 'defaultSystemOutput'] });
    assert.deepEqual(snapshot.sideEffects, {
      silenceProbe: { availability, enabledByDefault: false, sources: ['mic', 'system', 'process', 'mix'],
        condition: 'silentActiveSystemAudio' },
      classicLoopbackKeepalive: { availability, enabledByDefault: true, sources: ['system', 'mix'],
        condition: 'classicLoopbackWithoutExclusions' },
    });
  }
  const availability = snapshots.map(snapshot => snapshot.sources.mic.availability);
  assert.deepEqual(availability[0], { status: 'supported' });
  assert.deepEqual(availability[1], { status: 'unavailable', reason: { kind: 'unsupportedOs' } });
  assert.deepEqual(availability[2], { status: 'unavailable', reason: { kind: 'osVersionTooOld',
    required: { major: 14, minor: 4, patch: null, build: null },
    actual: { major: 14, minor: 3, patch: 1, build: 42 } } });
  assert.deepEqual(availability[3], { status: 'unavailable', reason: { kind: 'pipeWireUnavailable' } });
  assert.deepEqual(availability[4], { status: 'unavailable', reason: { kind: 'notApplicable' } });
  assert.equal(availability[5].status, 'unknown');
  assert.throws(() => native.__bindingContractError('wrapped'), error => {
    assert.deepEqual(availability[5].error, error.audioError);
    return true;
  });
});

test('capabilities returns a Promise with a platform snapshot', async () => {
  const pending = native.capabilities();
  assert.ok(pending instanceof Promise);
  const snapshot = await pending;
  assert.equal(typeof snapshot.sources.mic.backend, 'string');
  assert.ok(['supported', 'unavailable', 'unknown'].includes(snapshot.sources.system.availability.status));
  assert.equal(typeof snapshot.sideEffects.classicLoopbackKeepalive.enabledByDefault, 'boolean');
});

test('keepalive defaults on open and preserves or replaces the current value on switch', () => {
  const resolve = native.__bindingContractOpenOptions;
  assert.equal(resolve({ kind: 'system' }), true);
  for (const enabled of [true, false]) {
    assert.equal(resolve({ kind: 'system', windowsClassicLoopbackKeepalive: enabled }), enabled);
    assert.equal(resolve({ kind: 'system' }, enabled), enabled);
    for (const replacement of [true, false]) {
      assert.equal(resolve({ kind: 'system', windowsClassicLoopbackKeepalive: replacement }, enabled), replacement);
    }
  }
  for (const invalid of [0, 1, 'true', {}, []]) {
    assert.throws(() => resolve({ kind: 'system', windowsClassicLoopbackKeepalive: invalid }));
    assert.throws(() => resolve({ kind: 'system', windowsClassicLoopbackKeepalive: invalid }, false));
  }
});

test('stream entry points validate keepalive booleans and mock switches accept the option', async () => {
  const stream = native.__openMockStream(48000, 2, 0, () => {});
  try {
    stream.switchSource({ kind: 'mock' });
    for (const enabled of [false, true]) {
      stream.switchSource({ kind: 'mock', windowsClassicLoopbackKeepalive: enabled });
    }
    for (const invalid of [0, 1, 'false', {}, []]) {
      assert.throws(() => stream.switchSource({ kind: 'mock', windowsClassicLoopbackKeepalive: invalid }));
      // N-API rejects the type before openStream can acquire an audio device.
      assert.throws(() => native.openStream({ kind: 'mic', windowsClassicLoopbackKeepalive: invalid }, () => {}));
    }
  } finally {
    await stream.stop();
  }
});
