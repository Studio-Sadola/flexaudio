import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const require = createRequire(import.meta.url);
const native = require(resolve(process.env.FLEXAUDIO_TEST_ADDON || new URL('flexaudio.node', import.meta.url).pathname));
const row = (report, key) => {
  const entry = report.entries.find(entry => entry.setting === key);
  assert.ok(entry, `missing ${key}`);
  return entry;
};

test('denoise opens at noncanonical output rates with the shared settings report', async () => {
  for (const outputRate of [16000, 44100, 48000]) {
    const chunks = [];
    const stream = native.openStream({ kind: 'mock', outputRate, outputChannels: 1,
      denoise: true }, chunk => chunks.push(chunk));
    try {
      const report = stream.effectiveSettings();
      assert.equal(row(report, 'denoise').requested, true);
      assert.equal(row(report, 'denoise').effective, true);
      assert.deepEqual(row(report, 'denoise').outcome, { status: 'applied' });
      assert.deepEqual(row(report, 'output').effective, { sampleRate: outputRate, channels: 1 });
      assert.deepEqual(row(report, 'output').outcome, { status: 'applied' });
    } finally { await stream.stop(); }
    assert.ok(chunks.some(chunk => chunk.frames === outputRate / 50));
  }
});
const ignored = (options, keys) => {
  let report;
  assert.throws(() => native.openStream({ ...options, strict: true }, () => {}), error => {
    assert.ok(error instanceof Error);
    assert.equal(error.code, 'FLEX_IGNORED_SETTINGS');
    assert.equal(error.report.entries.length, 20);
    assert.equal(typeof error.report.revision, 'bigint');
    assert.equal(error.audioError, undefined);
    report = error.report;
    for (const [key, reason] of keys) assert.deepEqual(row(error.report, key).outcome, { status: 'ignored', reason });
    return true;
  });
  return report;
};

test('open defaults to strict and explicit false permits ignored settings and survives switches', async () => {
  assert.throws(() => native.openStream({ kind: 'mic', excludeSelf: true }, () => {}), error => {
    assert.equal(error.code, 'FLEX_IGNORED_SETTINGS');
    assert.equal(row(error.report, 'excludeSelf').outcome.status, 'ignored');
    return true;
  });
  const stream = native.openStream({ kind: 'mock', excludeSelf: true, strict: false }, () => {});
  try {
    assert.equal(row(stream.effectiveSettings(), 'excludeSelf').outcome.status, 'ignored');
    stream.switchSource({ kind: 'mock', excludeSelf: true });
    assert.equal(row(stream.effectiveSettings(), 'excludeSelf').outcome.status, 'ignored');
  } finally { await stream.stop(); }
});

test('default values use value comparison, and reports include all binding settings', async () => {
  const stream = native.openStream({ kind: 'mock', strict: true, gain: 1, ringCapacityChunks: 50,
    excludeSelf: false, excludePids: [], mode: 'include', micGain: 1, systemGain: 1,
    silenceProbe: true, windowsClassicLoopbackKeepalive: true, denoise: false }, () => {});
  try {
    const report = stream.effectiveSettings();
    assert.equal(report.revision, 0n);
    assert.equal(report.entries.length, 20);
    for (const entry of report.entries) assert.equal(entry.outcome.status, 'notRequested', entry.setting);
    assert.equal(row(report, 'ringCapacityChunks').requested, 50n);
    assert.equal(row(report, 'vad').requested, null);
    report.entries.length = 0;
    assert.equal(stream.effectiveSettings().entries.length, 20);
  } finally { await stream.stop(); }
  assert.equal(stream.effectiveSettings().revision, 0n);
});

test('strict matrix rejects all ignored non-default values with shared report keys', () => {
  ignored({ kind: 'mic', processId: 123, mode: 'exclude', excludeSelf: true, excludePids: [123],
    micDeviceId: 'input-selector', systemDeviceId: 'output-selector', micGain: 2, systemGain: 2,
    silenceProbe: false, windowsClassicLoopbackKeepalive: false }, [
    ['targetPid', 'wrongSourceKind'], ['mode', 'wrongSourceKind'], ['excludeSelf', 'wrongSourceKind'],
    ['excludePids', 'wrongSourceKind'], ['mixMicDeviceId', 'wrongSourceKind'],
    ['mixSystemDeviceId', 'wrongSourceKind'], ['mixMicGain', 'wrongSourceKind'],
    ['mixSystemGain', 'wrongSourceKind'], ['silenceProbe', 'wrongSourceKind'],
    ['windowsClassicLoopbackKeepalive', 'wrongSourceKind'],
  ]);
  ignored({ kind: 'process', processId: 123, deviceId: 'selector' }, [['deviceId', 'wrongSourceKind']]);
  ignored({ kind: 'mix', deviceId: 'selector' }, [['deviceId', 'wrongSourceKind']]);
  if (process.platform === 'linux') {
    const report = ignored({ kind: 'system', excludePids: [123, 123], deviceId: 'selector', silenceProbe: false,
      windowsClassicLoopbackKeepalive: false }, [['deviceId', 'exclusionDeviceUnscoped'],
      ['silenceProbe', 'unsupportedPlatform'], ['windowsClassicLoopbackKeepalive', 'unsupportedPlatform']]);
    assert.deepEqual(row(report, 'excludePids').outcome, { status: 'adjusted', reason: 'exclusionSetNormalized' });
    assert.deepEqual(row(report, 'excludePids').effective, [123]);
  }
});

test('switch revisions, strict retention, fixed values and failed-switch rollback', async () => {
  const stream = native.openStream({ kind: 'mock', strict: true, gain: 2, ringCapacityChunks: 7 }, () => {});
  try {
    stream.switchSource({ kind: 'mock', gain: 1, ringCapacityChunks: 50 });
    let report = stream.effectiveSettings();
    assert.equal(report.revision, 1n);
    assert.equal(row(report, 'gain').effective, 2);
    assert.equal(row(report, 'ringCapacityChunks').effective, 7n);
    for (const key of ['gain', 'ringCapacityChunks']) assert.equal(row(report, key).outcome.status, 'notRequested');
    assert.throws(() => stream.switchSource({ kind: 'mock', gain: 2, ringCapacityChunks: 7, denoise: true, vad: {} }), error => {
      assert.equal(error.code, 'FLEX_IGNORED_SETTINGS');
      for (const key of ['gain', 'ringCapacityChunks', 'denoise', 'vad']) assert.deepEqual(row(error.report, key).outcome, { status: 'ignored', reason: 'fixedAtOpen' });
      return true;
    });
    assert.deepEqual(stream.effectiveSettings(), report);
    stream.switchSource({ kind: 'mock', gain: 2, strict: false });
    stream.switchSource({ kind: 'mock', gain: 2 });
    report = stream.effectiveSettings();
    assert.equal(report.revision, 3n);
    assert.equal(row(report, 'gain').outcome.status, 'ignored');
    stream.setGain(3);
    stream.switchSource({ kind: 'mock' });
    assert.equal(row(stream.effectiveSettings(), 'gain').effective, 3);
  } finally { await stream.stop(); }
});

test('ring capacity rejects zero, fractional, nonfinite and unsafe values', () => {
  for (const ringCapacityChunks of [0, -1, 1.5, NaN, Infinity, 2 ** 53]) {
    assert.throws(() => native.openStream({ kind: 'mock', ringCapacityChunks }, () => {}), /positive safe integer/);
  }
});

test('lifecycle uses only its own FIFO and stop waits for final callbacks', async () => {
  const ordinary = [];
  const lifecycle = [];
  const chunks = [];
  const stream = native.openStream({ kind: 'mock', secondaryOutput: { rate: 16000, channels: 1 },
    onLifecycleEvent: event => lifecycle.push(event) }, chunk => chunks.push(chunk), event => ordinary.push(event));
  await new Promise(resolve => setTimeout(resolve, 140));
  await stream.stop();
  assert.deepEqual(lifecycle.map(e => e.type), ['silenceProbeStarted', 'classicLoopbackKeepaliveStarted', 'silenceProbeStopped', 'classicLoopbackKeepaliveStopped']);
  for (const event of lifecycle.slice(2)) assert.equal(event.reason, 'streamStopped');
  assert.ok(ordinary.every(e => !e.type.includes('Probe') && !e.type.includes('Keepalive') && e.type !== 'unknown'));
  assert.ok(chunks.some(chunk => chunk.frames > 0));
  assert.ok(chunks.some(chunk => chunk.secondary));
  for (const chunk of chunks) {
    assert.equal(chunk.arrivalNs, chunk.ptsNs);
    if (chunk.secondary) assert.equal(chunk.secondary.arrivalNs, chunk.secondary.ptsNs);
  }
  const count = lifecycle.length;
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(lifecycle.length, count);
});

test('switch retains lifecycle handler and replaces it only after successful commit', async () => {
  const first = [], second = [], rejected = [];
  const stream = native.openStream({ kind: 'mock', strict: true, onLifecycleEvent: e => first.push(e) }, () => {});
  try {
    stream.switchSource({ kind: 'mock' });
    assert.throws(() => stream.switchSource({ kind: 'mock', gain: 2, onLifecycleEvent: e => rejected.push(e) }));
    stream.switchSource({ kind: 'mock', onLifecycleEvent: e => second.push(e) });
  } finally { await stream.stop(); }
  assert.ok(first.some(e => e.type === 'silenceProbeStarted'));
  assert.ok(second.some(e => e.type === 'silenceProbeStopped'));
  assert.equal(rejected.length, 0);
});

test('lifecycle notifications are discarded when no handler is registered', async () => {
  const ordinary = [];
  const stream = native.openStream({ kind: 'mock' }, () => {}, event => ordinary.push(event));
  await stream.stop();
  assert.deepEqual(ordinary, []);
});

test('stop immediately rejects lifecycle handler switches without crashing', async () => {
  let rejectedCalls = 0;
  for (let iteration = 0; iteration < 100; iteration++) {
    const stream = native.openStream({ kind: 'mock', onLifecycleEvent: () => {} }, () => {});
    const stopped = stream.stop();
    // Keep the JS thread occupied while the bridge exits its command loop and
    // tears down capture. Every request must fail before retaining its handler.
    for (let attempt = 0; attempt < 20; attempt++) {
      assert.throws(() => stream.switchSource({ kind: 'mock',
        onLifecycleEvent: () => { rejectedCalls++; } }), /stream already stopped/);
    }
    await stopped;
    assert.throws(() => stream.switchSource({ kind: 'mock',
      onLifecycleEvent: () => { rejectedCalls++; } }), /stream already stopped/);
  }
  assert.equal(rejectedCalls, 0);
});

test('lifecycle handlers can request stop asynchronously and return', async () => {
  const events = [];
  let stopped;
  const stream = native.openStream({ kind: 'mock', onLifecycleEvent: event => {
    events.push(event);
    stopped ??= stream.stop();
  } }, () => {});
  // Delivery is asynchronous, so stream is initialized before the first handler.
  await new Promise(resolve => setImmediate(resolve));
  await (stopped ?? stream.stop());
  assert.deepEqual(events.map(event => event.type), ['silenceProbeStarted',
    'classicLoopbackKeepaliveStarted', 'silenceProbeStopped', 'classicLoopbackKeepaliveStopped']);
});

test('namespaced discovery is asynchronous and retains typed discovery errors', async () => {
  assert.equal(typeof native.devicesWithNamespaces, 'function');
  const promise = native.devicesWithNamespaces();
  assert.ok(promise instanceof Promise);
  try {
    const devices = await promise;
    for (const entry of devices) {
      assert.equal(typeof entry.device.id, 'string');
      assert.ok(['cpalDeviceName', 'pipeWireNodeName', 'wasapiOutputSelector', 'coreAudioOutputSelector', 'unknown'].includes(entry.idNamespace));
    }
  } catch (error) {
    assert.ok(error.code.startsWith('FLEX_'));
    assert.ok(error.audioError);
  }
});
