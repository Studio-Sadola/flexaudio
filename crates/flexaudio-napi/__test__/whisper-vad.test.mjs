// Build/copy the addon as described in run-smoke.sh, then run:
// node crates/flexaudio-napi/__test__/whisper-vad.test.mjs
// These tests never enumerate or open real audio devices.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';

const native = createRequire(import.meta.url)(process.env.FLEXAUDIO_TEST_ADDON || './flexaudio.node');
const { WhisperVad } = native;
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

test('standalone exports the exact discriminated event fields without capture origins', () => {
  const { events } = collect(new Float32Array(513), [513]);
  assert.deepEqual(events, [
    { type: 'provisionalSpeechStart', epoch: 0, seq: 0, atMs: 0 },
    { type: 'provisionalCut', epoch: 0, seq: 1, startMs: 0, endMs: 32, reason: 'finish' },
    { type: 'provisionalSpeechEnd', epoch: 0, seq: 2, atMs: 32, reason: 'finish' },
    { type: 'segment', epoch: 0, seq: 3, startMs: 0, endMs: 60 },
    { type: 'epochEnd', epoch: 0, seq: 4, reason: 'finish' },
  ]);
});

test('default silence yields only an epoch terminal and preview is opt-in', () => {
  const vad = new WhisperVad();
  assert.deepEqual(vad.process(new Float32Array(2048)), []);
  assert.deepEqual(vad.finish(), [{ type: 'epochEnd', epoch: 0, seq: 0, reason: 'finish' }]);
});

test('synthetic PCM events and probabilities are independent of feed partition', () => {
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

test('EOF infers exactly one partial tail; preview uses physical integer milliseconds', () => {
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

test('latest probabilities own their data and identify the latest frame batch', () => {
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

test('finish is idempotent and reset starts a new epoch without duplicate closure', () => {
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

test('running reset closes published hints and abandons pending final segments', () => {
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

test('provisional cuts use the fixed 30000 ms deadline including a partial EOF frame', () => {
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

test('invalid PCM throws a typed code and leaves the complete feed unconsumed', () => {
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

test('parameters reject unknown fields, nonboolean preview and unsafe numeric conversions', () => {
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
  { skip: typeof native.openStream !== 'function' && 'standalone boundary addon has no capture API' }, () => {
  const open = options => native.openStream({ kind: 'mic', ...options }, () => {});
  rejectsCode(() => open({ vad: {}, whisperVad: { tap: 'primary' } }), 'ConflictingVad');
  rejectsCode(() => open({ vadTap: 'secondary', secondaryOutput: { rate: 16000, channels: 1 },
    whisperVad: { tap: 'primary' } }), 'ConflictingVad');
  for (const tap of ['other', 'secondary']) {
    rejectsCode(() => open({ whisperVad: { tap } }), 'UnsupportedTap');
  }
  for (const whisperVad of [{}, { tap: 5 }, { tap: 'primary', threshold: 1.1 },
    { tap: 'primary', provisional: 'true' }, { tap: 'primary', params: {} }]) {
    rejectsCode(() => open({ whisperVad }), 'InvalidParameter');
  }
});

test('attachment fails closed without exact canonical capture provenance',
  { skip: typeof native.openStream !== 'function' && 'standalone boundary addon has no capture API' }, () => {
  const open = options => native.openStream({ kind: 'mic', ...options }, () => {});
  rejectsCode(() => open({ whisperVad: { tap: 'primary' } }), 'UnsupportedConversionClock');
  rejectsCode(() => open({ secondaryOutput: { rate: 16000, channels: 1, encoding: 's16' },
    whisperVad: { tap: 'secondary' } }), 'UnsupportedConversionClock');
});
