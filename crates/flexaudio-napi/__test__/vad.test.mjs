// Build/copy the addon as described in run-smoke.sh, then run:
// node crates/flexaudio-napi/__test__/vad.test.mjs
// These tests never enumerate or open real audio devices.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { capabilitySkip } from './require-capabilities.mjs';
import { native, releaseNative as plainNative } from './load-addon.mjs';
const { Vad } = plainNative;
const faultTests = {};
assert.equal(typeof Vad, 'function', 'release addon must export Vad');
const standaloneTest = test;
const speaking = { threshold: 0, minSpeechMs: 0, speechPadMs: 0, provisional: true };

function collect(pcm, sizes, params = speaking) {
  const vad = new Vad(params);
  const events = [];
  const probabilities = [];
  let offset = 0;
  let index = 0;
  while (offset < pcm.length) {
    const end = Math.min(pcm.length, offset + sizes[index++ % sizes.length]);
    events.push(...vad.process(pcm.subarray(offset, end)));
    probabilities.push(...vad.lastFrameProbabilities().values);
    offset = end;
  }
  events.push(...vad.finish());
  probabilities.push(...vad.lastFrameProbabilities().values);
  return { events, probabilities };
}

function rejectsCode(fn, code) {
  assert.throws(fn, error => {
    assert.equal(error.code, code);
    assert.deepEqual(error.terminalEvents, []);
    return true;
  });
}

standaloneTest('standalone exports the exact discriminated event fields without capture origins', () => {
  const { events } = collect(new Float32Array(513), [513]);
  assert.deepEqual(events, [
    { type: 'provisionalSpeechStart', epoch: 0, seq: 0, atMs: 0 },
    { type: 'provisionalCut', epoch: 0, seq: 1, startMs: 0, endMs: 32, reason: 'finish' },
    { type: 'provisionalSpeechEnd', epoch: 0, seq: 2, atMs: 32, reason: 'finish' },
    { type: 'segment', epoch: 0, seq: 3, startMs: 0, endMs: 60 },
    { type: 'epochEnd', epoch: 0, seq: 4, reason: 'finish' },
  ]);
});

standaloneTest('default silence yields only an epoch terminal and preview is opt-in', () => {
  const vad = new Vad();
  assert.deepEqual(vad.process(new Float32Array(2048)), []);
  assert.deepEqual(vad.finish(), [{ type: 'epochEnd', epoch: 0, seq: 0, reason: 'finish' }]);
});

standaloneTest('synthetic PCM events and probabilities are independent of feed partition', () => {
  const pcm = Float32Array.from({ length: 4097 }, (_, i) => 0.2 * Math.sin(i * 0.17));
  const whole = collect(pcm, [pcm.length]);
  for (const sizes of [[1], [320], [512], [3, 517, 17, 1600]]) {
    assert.deepEqual(collect(pcm, sizes), whole);
  }
  const finalsOnly = collect(pcm, [317], { ...speaking, provisional: false });
  assert.deepEqual(finalsOnly.probabilities, whole.probabilities);
  assert.deepEqual(finalsOnly.events.filter(e => e.type === 'segment').map(({ startMs, endMs }) => ({ startMs, endMs })),
    whole.events.filter(e => e.type === 'segment').map(({ startMs, endMs }) => ({ startMs, endMs })));
});

standaloneTest('EOF infers exactly one partial tail; preview uses physical integer milliseconds', () => {
  for (const length of [0, 1, 15, 16, 511, 512, 513]) {
    const result = collect(new Float32Array(length), [17]);
    assert.equal(result.probabilities.length, Math.ceil(length / 512));
    const end = result.events.find(e => e.type === 'provisionalSpeechEnd');
    if (length) assert.equal(end.atMs, Math.floor(length / 16));
    else assert.equal(end, undefined);
    for (const segment of result.events.filter(e => e.type === 'segment')) {
      assert.equal(segment.startMs % 10, 0);
      assert.equal(segment.endMs % 10, 0);
    }
  }
});

standaloneTest('latest probabilities own their data and identify the latest frame batch', () => {
  const vad = new Vad(speaking);
  vad.process(new Float32Array(512));
  const first = vad.lastFrameProbabilities();
  assert.equal(first.firstFrameIndex, 0);
  assert.ok(first.values instanceof Float32Array);
  assert.equal(first.values.length, 1);
  const value = first.values[0];
  first.values[0] = 99;
  assert.equal(vad.lastFrameProbabilities().values[0], value);
  vad.process(new Float32Array());
  assert.equal(vad.lastFrameProbabilities().firstFrameIndex, 1);
  assert.equal(vad.lastFrameProbabilities().values.length, 0);
  vad.process(new Float32Array(1));
  vad.finish();
  assert.equal(vad.lastFrameProbabilities().firstFrameIndex, 1);
  assert.equal(vad.lastFrameProbabilities().values.length, 1);
  assert.equal(first.values[0], 99);
});

standaloneTest('finish is idempotent and reset starts a new epoch without duplicate closure', () => {
  const vad = new Vad(speaking);
  vad.process(new Float32Array(513));
  vad.finish();
  assert.deepEqual(vad.finish(), []);
  rejectsCode(() => vad.process(new Float32Array()), 'SessionFinished');
  assert.deepEqual(vad.reset(), []);
  assert.deepEqual(vad.process(new Float32Array(512)), [
    { type: 'provisionalSpeechStart', epoch: 1, seq: 0, atMs: 0 },
  ]);
});

standaloneTest('running reset closes published hints and abandons pending final segments', () => {
  const vad = new Vad(speaking);
  vad.process(new Float32Array(513));
  assert.deepEqual(vad.reset(), [
    { type: 'provisionalCut', epoch: 0, seq: 1, startMs: 0, endMs: 32, reason: 'reset' },
    { type: 'provisionalSpeechEnd', epoch: 0, seq: 2, atMs: 32, reason: 'reset' },
    { type: 'epochEnd', epoch: 0, seq: 3, reason: 'reset' },
  ]);
  assert.equal(vad.lastFrameProbabilities().values.length, 0);
  assert.equal(vad.lastFrameProbabilities().firstFrameIndex, 0);
});

standaloneTest('provisional cuts use the fixed 30000 ms deadline including a partial EOF frame', () => {
  const { events } = collect(new Float32Array(16 * 30001), [1703]);
  const cuts = events.filter(e => e.type === 'provisionalCut');
  assert.deepEqual(cuts.map(({ startMs, endMs, reason }) => ({ startMs, endMs, reason })), [
    { startMs: 0, endMs: 30000, reason: 'limit' },
    { startMs: 30000, endMs: 30001, reason: 'finish' },
  ]);
  assert.equal(events.filter(e => e.type === 'provisionalSpeechStart').length, 1);
  assert.equal(events.filter(e => e.type === 'provisionalSpeechEnd').length, 1);
  events.forEach((event, seq) => assert.equal(event.seq, seq));
  assert.equal(events.at(-1).type, 'epochEnd');
});

standaloneTest('invalid PCM throws a typed code and leaves the complete feed unconsumed', () => {
  const vad = new Vad(speaking);
  const control = new Vad(speaking);
  const prefix = new Float32Array(319);
  assert.deepEqual(vad.process(prefix), control.process(prefix));
  for (const sample of [NaN, Infinity, -Infinity, 1.01, -1.01]) {
    const input = new Float32Array(513);
    input[512] = sample;
    rejectsCode(() => vad.process(input), 'InvalidPcm');
  }
  for (const input of [[], new Float64Array(512), new Int16Array(512), {}, null]) {
    rejectsCode(() => vad.process(input), 'InvalidPcm');
  }
  const suffix = new Float32Array(194);
  assert.deepEqual(vad.process(suffix), control.process(suffix));
  assert.deepEqual(vad.finish(), control.finish());
});

standaloneTest('parameters reject unknown fields, nonboolean preview and unsafe numeric conversions', () => {
  const invalid = [
    { threshold: 1.0000000001 }, { threshold: NaN }, { threshold: Infinity }, { threshold: -0.1 },
    { maxSpeechMs: Infinity }, { maxSpeechMs: -1 }, { maxSpeechMs: 4294967296 },
    { minSpeechMs: -1 }, { minSpeechMs: 0.5 }, { minSpeechMs: true },
    { minSilenceMs: 134218 }, { speechPadMs: 4294967296 },
    { speechPadMs: null }, { provisional: 1 }, { provisional: 'true' }, { provisional: null },
    { negThreshold: 0.2 }, { sampleRate: 8000 }, { min_speech_ms: 250 },
    { [Symbol('unknown')]: 1 }, Object.defineProperty({}, 'unknown', { value: 1 }),
    null, [], 3, 'options',
  ];
  for (const params of invalid) rejectsCode(() => new Vad(params), 'InvalidParameter');
  const vad = new Vad({ threshold: 0, minSpeechMs: 0, minSilenceMs: 0,
    maxSpeechMs: 0, speechPadMs: 0 });
  vad.finish();
});

test('stream options reject unsupported taps and malformed values before opening devices', () => {
  const open = options => plainNative.openStream({ kind: 'mic', ...options }, () => {});
  for (const tap of ['other', 'secondary']) {
    rejectsCode(() => open({ vad: { tap } }), 'UnsupportedTap');
  }
  for (const vad of [{}, { tap: 5 }, { tap: 'primary', threshold: 1.1 },
    { tap: 'primary', provisional: 'true' }, { tap: 'primary', params: { threshold: 1.1 } }]) {
    rejectsCode(() => open({ vad }), 'InvalidParameter');
  }
});

function waitFor(predicate, message, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  return (async () => {
    while (!predicate()) {
      assert.ok(Date.now() < deadline, message);
      await new Promise(resolve => setTimeout(resolve, 5));
    }
  })();
}

function openVADMock(t, log, failAtChunk = 3, resetFailures = undefined) {
  const stream = native.__openMockStream(48000, 2, 0,
    chunk => log.push({ kind: 'chunk', frames: chunk.frames, events: chunk.vadEvents ?? [] }),
    undefined, undefined, undefined,
    { params: { threshold: 0, minSpeechMs: 0, speechPadMs: 0 }, provisional: true, tap: 'primary' },
    event => log.push({ kind: 'event', event }), failAtChunk, resetFailures);
  // Register before assertions: a failed assertion must never strand the capture worker/TSFN.
  t.after(async () => { await stream.stop(); });
  return stream;
}

function failureIndex(log) {
  return log.findIndex(entry => entry.kind === 'event' && entry.event.type === 'vadFailed');
}

function assertFailureOrder(log, before = log.length) {
  const failure = failureIndex(log);
  assert.ok(failure >= 0 && failure < before, 'vadFailed must arrive before settlement');
  const event = log[failure].event;
  const carrier = log.findIndex(entry => entry.kind === 'chunk'
    && entry.events.some(item => item.type === 'epochEnd' && item.reason === 'error' && item.epoch === event.epoch));
  assert.ok(carrier >= 0 && carrier < failure, 'epochEnd(error) carrier must precede vadFailed');
  assert.equal(log[carrier].events.at(-1).type, 'epochEnd');
  assert.equal(event.error.kind, 'backend');
  assert.equal(event.error.vadCode, 'Inference');
  assert.match(event.error.message, /VAD/i);
  assert.equal(event.error.secondary.length, 0);
  return event;
}

function assertVADReport(stream, code = 'Inference', count = 1) {
  const report = stream.shutdownReport();
  assert.equal(report.primary, null, 'an attached VAD-only failure is never the capture primary');
  const attachment = report.cleanupErrors.filter(error => typeof error.vadCode === 'string');
  assert.equal(attachment.length, count, 'distinct failures are retained once as cleanup causes');
  assert.equal(attachment[0].vadCode, code);
  assert.equal(stream.terminalError(), undefined, 'terminalError means capture primary only');
}

test('release native addon excludes test controls', () => {
  assert.equal(Object.hasOwn(plainNative, '__openMockStream'), false);
  assert.equal(Object.hasOwn(plainNative, '__vadTestFaults'), false);
  assert.equal(Object.hasOwn(plainNative, '__bindingContractBridge'), false);
});

test('mock failure counts reject fractional, negative and overflowing values before capture starts', faultTests, async () => {
  for (const [failure, resets] of [[0, undefined], [-1, undefined], [1.5, undefined], [2 ** 32 + 3, undefined],
    [undefined, -1], [undefined, 0.5], [undefined, Infinity]]) {
    let unexpected;
    try {
      rejectsCode(() => { unexpected = native.__openMockStream(48000, 2, 0, () => {},
        undefined, undefined, undefined, { tap: 'primary' }, undefined,
        failure, resets); }, 'InvalidParameter');
    } finally {
      if (unexpected) await unexpected.stop();
    }
  }
});

test('an attached VAD-only failure never fails capture or emits a stop terminator before stop', faultTests, async t => {
  const log = [];
  let uncaught;
  const onUncaught = error => { uncaught = error; };
  process.once('uncaughtException', onUncaught);
  t.after(() => process.removeListener('uncaughtException', onUncaught));
  const stream = openVADMock(t, log);
  await waitFor(() => failureIndex(log) >= 0, 'an attached VAD-only failure must be reported as vadFailed');
  assert.equal(uncaught, undefined, 'an attached VAD-only failure must not become an uncaughtException');
  const event = assertFailureOrder(log);
  const failure = failureIndex(log);
  await waitFor(() => log.slice(failure).filter(entry => entry.kind === 'chunk' && entry.frames > 0).length > 3,
    'PCM delivery must continue after an attached VAD-only failure');
  assert.equal(log.some(entry => entry.kind === 'chunk' && entry.frames === 0), false,
    'a mid-stream failure carrier must use real PCM, because frames:0 means stop');
  assert.doesNotThrow(() => stream.resume(), 'resume must succeed after an attached VAD-only failure');
  await stream.stop();
  log.push({ kind: 'resolved' });
  assertFailureOrder(log, log.length - 1);
  assertVADReport(stream, event.error.vadCode);
  assert.equal(log.filter(entry => entry.kind === 'chunk').at(-1).frames, 0, 'stop delivers a terminator');
});

test('an immediate stop from the error carrier observes epochEnd then failure event then stop resolution', faultTests, async t => {
  const log = [];
  let stopping;
  let stream;
  stream = native.__openMockStream(48000, 2, 0, chunk => {
    const events = chunk.vadEvents ?? [];
    log.push({ kind: 'chunk', frames: chunk.frames, events });
    if (events.some(event => event.type === 'epochEnd' && event.reason === 'error')) {
      stopping ??= stream.stop().then(() => { log.push({ kind: 'resolved' }); });
    }
  }, undefined, undefined, undefined,
  { params: { threshold: 0, minSpeechMs: 0, speechPadMs: 0 }, provisional: true, tap: 'primary' },
  event => log.push({ kind: 'event', event }), 3);
  t.after(async () => { await stream.stop(); });
  await waitFor(() => stopping !== undefined, 'the real failure must deliver epochEnd(error)');
  await stopping;
  assertFailureOrder(log, log.findIndex(entry => entry.kind === 'resolved'));
  assertVADReport(stream);
});

for (const boundary of ['resume', 'switchSource']) {
  test(`the discontinuity after ${boundary} reinitializes the failed session with a new origin`, faultTests, async t => {
    const log = [];
    const stream = openVADMock(t, log);
    await waitFor(() => failureIndex(log) >= 0, 'an attached VAD-only failure must be reported');
    const failedEpoch = log[failureIndex(log)].event.epoch;
    const boundaryIndex = log.length;
    if (boundary === 'resume') {
      stream.pause();
      assert.equal(stream.isPaused(), true);
      assert.doesNotThrow(() => stream.resume());
    } else {
      assert.doesNotThrow(() => stream.switchSource({ kind: 'mock' }), 'switchSource must succeed after an attached VAD-only failure');
    }
    const origins = () => log.slice(boundaryIndex).flatMap(entry => entry.kind === 'chunk' ? entry.events : [])
      .filter(event => event.type === 'epochStart' && event.epoch > failedEpoch);
    await waitFor(() => origins().length > 0, `the discontinuity after ${boundary} must start a new epoch`);
    const origin = origins()[0];
    assert.equal(origin.seq, 0);
    assert.equal(typeof origin.captureSample, 'bigint');
    assert.ok(origin.captureSample > 0n, 'the new epoch anchors on the first post-gap frame');
    assert.equal(typeof origin.ptsNs, 'number');
    assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'vadFailed').length, 1);
    await stream.stop();
    assertVADReport(stream);
  });
}

test('one failed reset at a real boundary stays failed until explicit flush succeeds', faultTests, async t => {
  const log = [];
  const stream = openVADMock(t, log, 3, 1);
  await waitFor(() => failureIndex(log) >= 0, 'a real injected model failure must be reported');
  const failedEpoch = log[failureIndex(log)].event.epoch;
  const delivered = () => log.filter(entry => entry.kind === 'chunk' && entry.frames > 0).length;
  const before = delivered();
  await waitFor(() => delivered() >= before + 3, 'PCM must continue after the model failure');
  stream.pause();
  stream.resume();
  const afterBoundary = delivered();
  await waitFor(() => delivered() >= afterBoundary + 8, 'PCM must continue after the one failed reset');
  const newOrigins = () => log.flatMap(entry => entry.kind === 'chunk' ? entry.events : [])
    .filter(event => event.type === 'epochStart' && event.epoch > failedEpoch);
  assert.equal(newOrigins().length, 0, 'a failed reset must not publish a new origin on later contiguous chunks');
  assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'vadFailed').length, 2,
    'the distinct reset failure must be reported after the original failure');
  await stream.flushVad();
  await waitFor(() => newOrigins().length > 0, 'successful explicit flush must permit the next epochStart');
  assert.equal(newOrigins()[0].seq, 0);
  await stream.stop();
  assertVADReport(stream, 'Inference', 2);
});

test('flushVad rejects with the new reset failure and retains both failures', faultTests, async t => {
  const log = [];
  const stream = openVADMock(t, log, 3, 1);
  await waitFor(() => failureIndex(log) >= 0, 'the initial model failure must be delivered');
  await assert.rejects(stream.flushVad(), error => {
    assert.equal(error.code, 'Inference');
    assert.equal(error.audioError.secondary[0].addonKind, 'vadReset');
    return true;
  });
  const failures = log.filter(entry => entry.kind === 'event' && entry.event.type === 'vadFailed');
  assert.equal(failures.length, 2);
  assert.equal(failures[0].event.error.contexts[0].operation, 'normalize');
  assert.equal(failures[1].event.error.contexts[0].operation, 'flush');
  assert.equal(failures[1].event.error.secondary[0].addonKind, 'vadReset');
  await stream.stop();
  assertVADReport(stream, 'Inference', 2);
});

test('attached primary and secondary close epochs before flush and stop settle',
  { skip: capabilitySkip(typeof native.__openMockStream !== 'function',
    'the addon was built without the test-faults feature, so the fake capture API is unavailable') }, async t => {
  for (const tap of ['primary', 'secondary']) {
    const chunks = [];
    const events = [];
    const stream = native.__openMockStream(48000, 2, 0, chunk => {
      chunks.push(chunk);
      const carrier = tap === 'primary' ? chunk : chunk.secondary;
      if (carrier) events.push(...(carrier.vadEvents ?? []));
    }, tap === 'secondary' ? 16000 : undefined, 1, 's16',
    { params: { threshold: 0, minSpeechMs: 0, speechPadMs: 0 }, provisional: true, tap });
    t.after(async () => { await stream.stop(); });
    const deadline = Date.now() + 5000;
    while (!events.some(event => event.type === 'provisionalSpeechStart')) {
      assert.ok(Date.now() < deadline, 'fake capture timed out');
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.equal(events[0].type, 'epochStart');
    assert.equal(typeof events[0].captureSample, 'bigint');
    assert.equal(events[0].captureSample, 0n);
    assert.equal(typeof chunks[0].frameIndex, 'bigint');
    await stream.flushVad();
    assert.equal(events.at(-1).type, 'epochEnd');
    assert.equal(chunks.at(-1).frames, 0);
    await stream.stop();
    assert.equal(chunks.at(-1).frames, 0);
    await stream.stop();
    const epochs = new Map();
    for (const event of events) {
      const epoch = epochs.get(event.epoch) ?? [];
      epoch.push(event);
      epochs.set(event.epoch, epoch);
    }
    for (const epoch of epochs.values()) {
      assert.equal(epoch[0].type, 'epochStart');
      assert.equal(epoch.at(-1).type, 'epochEnd');
      epoch.forEach((event, seq) => assert.equal(event.seq, seq));
    }
  }
});

standaloneTest('standalone input rate and channels are strict optional format values', () => {
  const vad = new Vad(speaking);
  for (const [rate, channels] of [[0, 1], [16000.5, 1], [NaN, 1], [16000, 0], [16000, 1.5]]) {
    rejectsCode(() => vad.process(new Float32Array(512), rate, channels), 'InvalidParameter');
  }
  const mono = new Vad(speaking);
  const stereo = new Vad(speaking);
  const pcm = new Float32Array(16000);
  const expanded = new Float32Array(48000 * 2);
  const monoEvents = [...mono.process(pcm), ...mono.finish()];
  const stereoEvents = [...stereo.process(expanded, 48000, 2), ...stereo.finish()];
  const onset = events => events.find(event => event.type === 'segment').startMs;
  assert.ok(Math.abs(onset(monoEvents) - onset(stereoEvents)) <= 32);
});

standaloneTest('millisecond split budgets reject unsupported values without truncation', () => {
  for (const maxSpeechMs of [1, 32, 91, 900.5, -1, Infinity]) {
    rejectsCode(() => new Vad({ maxSpeechMs }), 'InvalidParameter');
  }
  for (const maxSpeechMs of [92, 93, 900, 1000, 0, 100001000]) {
    const vad = new Vad({ maxSpeechMs });
    vad.finish();
  }
  const zeroBudget = new Vad({ maxSpeechMs: 1000, speechPadMs: 484 });
  assert.deepEqual(zeroBudget.finish(), [{ type: 'epochEnd', epoch: 0, seq: 0, reason: 'finish' }]);
});

standaloneTest('valid full-scale source PCM survives finite resampling overshoot', () => {
  const vad = new Vad();
  const samples = Float32Array.from({ length: 9600 }, (_, index) => index < 4800 ? 1 : -1);
  assert.doesNotThrow(() => vad.process(samples, 48000, 2));
  assert.doesNotThrow(() => vad.finish());
});
