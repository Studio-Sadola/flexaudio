// Build/copy the addon as described in run-smoke.sh, then run:
// node crates/flexaudio-napi/__test__/whisper-vad.test.mjs
// These tests never enumerate or open real audio devices.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const plainNative = require(process.env.FLEXAUDIO_TEST_ADDON || './flexaudio.node');
const native = process.env.FLEXAUDIO_REPRO_ADDON ? require(process.env.FLEXAUDIO_REPRO_ADDON) : plainNative;
const { WhisperVad } = plainNative;
const faultTests = { skip: typeof native.__reproP9Bridge !== 'function' && !native.__whisperVadTestFaults && 'production addon excludes test fault injection' };
if (process.env.FLEXAUDIO_RUN_REPRO === '1') {
  assert.equal(faultTests.skip, false, 'required reproduction addon must support attached fault injection');
  assert.equal(typeof WhisperVad, 'function', 'required plain addon must export standalone WhisperVad');
  assert.equal(typeof plainNative.__openMockStream, 'function', 'required plain addon must export mock capture');
  assert.equal(Boolean(plainNative.__whisperVadTestFaults || plainNative.__reproP9Bridge), false,
    'plain addon must exclude fault injection so production rejection is exercised');
}
const standaloneTest = typeof WhisperVad === 'function' ? test
  : (name, fn) => test(name, { skip: 'reproduction addon has no standalone WhisperVad export; attached tests still run' }, fn);
const speaking = { threshold: 0, minSpeechDurationMs: 0, speechPadMs: 0, provisional: true };

function collect(pcm, sizes, params = speaking) {
  const vad = new WhisperVad(params);
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
  const vad = new WhisperVad();
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
  const vad = new WhisperVad(speaking);
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
  const vad = new WhisperVad(speaking);
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
  const vad = new WhisperVad(speaking);
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
  const vad = new WhisperVad(speaking);
  const control = new WhisperVad(speaking);
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
    { maxSpeechDurationS: Infinity }, { maxSpeechDurationS: -1 }, { maxSpeechDurationS: 3.5e38 },
    { minSpeechDurationMs: -1 }, { minSpeechDurationMs: 0.5 }, { minSpeechDurationMs: true },
    { minSilenceDurationMs: 134218 }, { speechPadMs: 4294967296 },
    { speechPadMs: null }, { provisional: 1 }, { provisional: 'true' }, { provisional: null },
    { negThreshold: 0.2 }, { sampleRate: 8000 }, { min_speech_duration_ms: 250 },
    { [Symbol('unknown')]: 1 }, Object.defineProperty({}, 'unknown', { value: 1 }),
    null, [], 3, 'options',
  ];
  for (const params of invalid) rejectsCode(() => new WhisperVad(params), 'InvalidParameter');
  const vad = new WhisperVad({ threshold: 0, minSpeechDurationMs: 0, minSilenceDurationMs: 0,
    maxSpeechDurationS: 0, speechPadMs: 0 });
  vad.finish();
});

test('stream options reject conflicts and unsupported taps before opening any device',
  { skip: typeof plainNative.openStream !== 'function' && 'standalone boundary addon has no capture API' }, () => {
  const open = options => plainNative.openStream({ kind: 'mic', ...options }, () => {});
  rejectsCode(() => open({ vad: {}, whisperVad: { tap: 'primary' } }), 'ConflictingVad');
  rejectsCode(() => open({ vadTap: 'secondary', secondaryOutput: { rate: 16000, channels: 1 },
    whisperVad: { tap: 'primary' } }), 'ConflictingVad');
  for (const tap of ['other', 'secondary']) {
    rejectsCode(() => open({ whisperVad: { tap } }), 'UnsupportedTap');
  }
  for (const whisperVad of [{}, { tap: 5 }, { tap: 'primary', threshold: 1.1 },
    { tap: 'primary', provisional: 'true' }, { tap: 'primary', params: { threshold: 1.1 } }]) {
    rejectsCode(() => open({ whisperVad }), 'InvalidParameter');
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

function openWhisperMock(t, log, failAtChunk = 3, resetFailures = undefined) {
  const stream = native.__openMockStream(48000, 2, 0,
    chunk => log.push({ kind: 'chunk', frames: chunk.frames, events: chunk.whisperVadEvents ?? [] }),
    undefined, undefined, undefined, undefined, undefined,
    { params: { threshold: 0, minSpeechDurationMs: 0, speechPadMs: 0 }, provisional: true, tap: 'primary' },
    event => log.push({ kind: 'event', event }), failAtChunk, resetFailures);
  // Register before assertions: a failed assertion must never strand the capture worker/TSFN.
  t.after(async () => { await stream.stop(); });
  return stream;
}

function failureIndex(log) {
  return log.findIndex(entry => entry.kind === 'event' && entry.event.type === 'whisperVadFailed');
}

function assertFailureOrder(log, before = log.length) {
  const failure = failureIndex(log);
  assert.ok(failure >= 0 && failure < before, 'whisperVadFailed must arrive before settlement');
  const event = log[failure].event;
  const carrier = log.findIndex(entry => entry.kind === 'chunk'
    && entry.events.some(item => item.type === 'epochEnd' && item.reason === 'error' && item.epoch === event.epoch));
  assert.ok(carrier >= 0 && carrier < failure, 'epochEnd(error) carrier must precede whisperVadFailed');
  assert.equal(log[carrier].events.at(-1).type, 'epochEnd');
  assert.equal(event.error.kind, 'backend');
  assert.equal(event.error.whisperCode, 'Inference');
  assert.match(event.error.message, /whisper/i);
  assert.equal(event.error.secondary.length, 0);
  return event;
}

function assertWhisperReport(stream, code = 'Inference') {
  const report = stream.shutdownReport();
  assert.equal(report.primary, null, 'a whisper-only failure is never the capture primary');
  const whisper = report.cleanupErrors.filter(error => typeof error.whisperCode === 'string');
  assert.equal(whisper.length, 1, 'the failure is retained once as a cleanup cause');
  assert.equal(whisper[0].whisperCode, code);
  assert.equal(stream.terminalError(), undefined, 'terminalError means capture primary only');
}

test('production mock capture rejects test faults because injection is not compiled into the addon',
  { skip: Boolean(plainNative.__whisperVadTestFaults || plainNative.__reproP9Bridge)
    || typeof plainNative.__openMockStream !== 'function' }, async () => {
  for (const [failAt, resetFailures] of [[3, undefined], [undefined, 1]]) {
    let unexpected;
    try {
      assert.throws(() => {
        unexpected = plainNative.__openMockStream(48000, 2, 0, () => {},
          undefined, undefined, undefined, undefined, undefined,
          { tap: 'primary' }, undefined, failAt, resetFailures);
      }, error => {
        assert.equal(error.code, 'TestFaultsUnavailable');
        assert.deepEqual(error.terminalEvents, []);
        assert.match(error.message, /injection.*not compiled|not compiled.*injection/i);
        return true;
      });
    } finally {
      // If a faulty addon accepts injection, the assertion is red and it still gets stopped.
      if (unexpected) await unexpected.stop();
    }
  }
});

test('mock failure counts reject fractional, negative and overflowing values before capture starts', faultTests, async () => {
  for (const [failure, resets] of [[0, undefined], [-1, undefined], [1.5, undefined], [2 ** 32 + 3, undefined],
    [undefined, -1], [undefined, 0.5], [undefined, Infinity]]) {
    let unexpected;
    try {
      rejectsCode(() => { unexpected = native.__openMockStream(48000, 2, 0, () => {},
        undefined, undefined, undefined, undefined, undefined, { tap: 'primary' }, undefined,
        failure, resets); }, 'InvalidParameter');
    } finally {
      if (unexpected) await unexpected.stop();
    }
  }
});

test('a whisper-only failure never fails capture or emits a stop terminator before stop', faultTests, async t => {
  const log = [];
  let uncaught;
  const onUncaught = error => { uncaught = error; };
  process.once('uncaughtException', onUncaught);
  t.after(() => process.removeListener('uncaughtException', onUncaught));
  const stream = openWhisperMock(t, log);
  await waitFor(() => failureIndex(log) >= 0, 'a whisper-only failure must be reported as whisperVadFailed');
  assert.equal(uncaught, undefined, 'a whisper-only failure must not become an uncaughtException');
  const event = assertFailureOrder(log);
  const failure = failureIndex(log);
  await waitFor(() => log.slice(failure).filter(entry => entry.kind === 'chunk' && entry.frames > 0).length > 3,
    'PCM delivery must continue after a whisper-only failure');
  assert.equal(log.some(entry => entry.kind === 'chunk' && entry.frames === 0), false,
    'a mid-stream failure carrier must use real PCM, because frames:0 means stop');
  assert.doesNotThrow(() => stream.resume(), 'resume must succeed after a whisper-only failure');
  await stream.stop();
  log.push({ kind: 'resolved' });
  assertFailureOrder(log, log.length - 1);
  assertWhisperReport(stream, event.error.whisperCode);
  assert.equal(log.filter(entry => entry.kind === 'chunk').at(-1).frames, 0, 'stop delivers a terminator');
});

test('an immediate stop from the error carrier observes epochEnd then failure event then stop resolution', faultTests, async t => {
  const log = [];
  let stopping;
  let stream;
  stream = native.__openMockStream(48000, 2, 0, chunk => {
    const events = chunk.whisperVadEvents ?? [];
    log.push({ kind: 'chunk', frames: chunk.frames, events });
    if (events.some(event => event.type === 'epochEnd' && event.reason === 'error')) {
      stopping ??= stream.stop().then(() => { log.push({ kind: 'resolved' }); });
    }
  }, undefined, undefined, undefined, undefined, undefined,
  { params: { threshold: 0, minSpeechDurationMs: 0, speechPadMs: 0 }, provisional: true, tap: 'primary' },
  event => log.push({ kind: 'event', event }), 3);
  t.after(async () => { await stream.stop(); });
  await waitFor(() => stopping !== undefined, 'the real failure must deliver epochEnd(error)');
  await stopping;
  assertFailureOrder(log, log.findIndex(entry => entry.kind === 'resolved'));
  assertWhisperReport(stream);
});

for (const boundary of ['resume', 'switchSource']) {
  test(`the discontinuity after ${boundary} reinitializes the failed session with a new origin`, faultTests, async t => {
    const log = [];
    const stream = openWhisperMock(t, log);
    await waitFor(() => failureIndex(log) >= 0, 'a whisper-only failure must be reported');
    const failedEpoch = log[failureIndex(log)].event.epoch;
    const boundaryIndex = log.length;
    if (boundary === 'resume') {
      stream.pause();
      assert.equal(stream.isPaused(), true);
      assert.doesNotThrow(() => stream.resume());
    } else {
      assert.doesNotThrow(() => stream.switchSource({ kind: 'mock' }), 'switchSource must succeed after a whisper-only failure');
    }
    const origins = () => log.slice(boundaryIndex).flatMap(entry => entry.kind === 'chunk' ? entry.events : [])
      .filter(event => event.type === 'epochStart' && event.epoch > failedEpoch);
    await waitFor(() => origins().length > 0, `the discontinuity after ${boundary} must start a new epoch`);
    const origin = origins()[0];
    assert.equal(origin.seq, 0);
    assert.equal(typeof origin.captureSample, 'bigint');
    assert.ok(origin.captureSample > 0n, 'the new epoch anchors on the first post-gap frame');
    assert.equal(typeof origin.ptsNs, 'number');
    assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'whisperVadFailed').length, 1);
    await stream.stop();
    assertWhisperReport(stream);
  });
}

test('one failed reset at a real boundary stays failed until explicit flush succeeds', faultTests, async t => {
  const log = [];
  const stream = openWhisperMock(t, log, 3, 1);
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
  assert.equal(log.filter(entry => entry.kind === 'event' && entry.event.type === 'whisperVadFailed').length, 1,
    'a failed reinitialization must not repeat the failure event');
  await stream.flushWhisperVad();
  await waitFor(() => newOrigins().length > 0, 'successful explicit flush must permit the next epochStart');
  assert.equal(newOrigins()[0].seq, 0);
  await stream.stop();
  assertWhisperReport(stream);
});

test('attached primary and secondary close epochs before flush and stop settle',
  { skip: typeof native.__openMockStream !== 'function' && 'addon has no fake capture API' }, async t => {
  for (const tap of ['primary', 'secondary']) {
    const chunks = [];
    const events = [];
    const stream = native.__openMockStream(48000, 2, 0, chunk => {
      chunks.push(chunk);
      const carrier = tap === 'primary' ? chunk : chunk.secondary;
      if (carrier) events.push(...(carrier.whisperVadEvents ?? []));
    }, tap === 'secondary' ? 16000 : undefined, 1, 's16', undefined, undefined,
    { params: { threshold: 0, minSpeechDurationMs: 0, speechPadMs: 0 }, provisional: true, tap });
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
    await stream.flushWhisperVad();
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
