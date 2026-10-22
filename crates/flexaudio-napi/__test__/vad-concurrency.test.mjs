// Build both addons as in run-smoke.sh; select the plain addon with FLEXAUDIO_RELEASE_ADDON.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';
import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { native, releaseNative } from './load-addon.mjs';

/** @typedef {import('../index.js').VadEvent} VadEvent */
/** @typedef {import('../index.js').AttachedVadEvent} AttachedVadEvent */
/** @typedef {import('../index.js').JsAudioChunk} AudioChunk */

const { Vad } = releaseNative;
const options = { threshold: 0.5, minSpeechMs: 100, minSilenceMs: 100,
  speechPadMs: 30, provisional: true };
const blockSize = 4093; // Deliberately leave partial 512-sample model frames.

function speechFiles() {
  const wav = readFileSync(new URL('../../flexaudio-vad/tests/fixtures/jp_2spk_FF_4s_16k.wav', import.meta.url));
  assert.equal(wav.toString('ascii', 0, 4), 'RIFF');
  assert.equal(wav.readUInt32LE(4) + 8, wav.length);
  assert.equal(wav.toString('ascii', 8, 12), 'WAVE');
  let formatSeen = false;
  /** @type {Float32Array | undefined} */
  let speech;
  for (let offset = 12; offset + 8 <= wav.length;) {
    const name = wav.toString('ascii', offset, offset + 4);
    const length = wav.readUInt32LE(offset + 4);
    offset += 8;
    assert.ok(offset + length <= wav.length, 'WAV chunk must fit in the fixture');
    if (name === 'fmt ') {
      assert.ok(length >= 16);
      assert.equal(wav.readUInt16LE(offset), 1, 'PCM format');
      assert.equal(wav.readUInt16LE(offset + 2), 1, 'mono');
      assert.equal(wav.readUInt32LE(offset + 4), 16000);
      assert.equal(wav.readUInt16LE(offset + 12), 2, 'sample alignment');
      assert.equal(wav.readUInt16LE(offset + 14), 16);
      formatSeen = true;
    } else if (name === 'data') {
      assert.ok(formatSeen);
      assert.equal(speech, undefined, 'exactly one PCM chunk');
      assert.equal(length % 2, 0);
      speech = Float32Array.from({ length: length / 2 }, (_, i) => wav.readInt16LE(offset + i * 2) / 32768);
    }
    offset += length + length % 2;
  }
  assert.ok(speech instanceof Float32Array);
  assert.equal(speech.length, 64000);
  // Three speech bursts separated by one second of silence require several finals.
  const makeFile = (gain, prefix) => {
    const pcm = new Float32Array(prefix + 3 * (speech.length + 16000) + 17);
    for (let burst = 0; burst < 3; burst++) {
      pcm.set(speech.map(sample => sample * gain), prefix + burst * (speech.length + 16000));
    }
    return pcm;
  };
  return [makeFile(1, 0), makeFile(0.75, 8000)];
}

/** @param {import('../index.js').Vad} vad @param {Float32Array} pcm */
function* feed(vad, pcm) {
  for (let offset = 0; offset < pcm.length; offset += blockSize) {
    yield vad.process(pcm.subarray(offset, offset + blockSize));
  }
  yield vad.finish();
}

/** @param {Float32Array[]} files @param {boolean} concurrent @param {Int32Array} progress @param {number} lane */
async function runFiles(files, concurrent, progress, lane, params = options) {
  const vad = new Vad(params);
  /** @type {VadEvent[][]} */
  const results = [];
  for (const [index, pcm] of files.entries()) {
    if (index) assert.deepEqual(vad.reset(), [], 'finished file must not close twice');
    /** @type {VadEvent[]} */
    const events = [];
    for (const batch of feed(vad, pcm)) {
      events.push(...batch);
      Atomics.add(progress, lane, 1);
      if (concurrent) await delay(2); // Keep capture callbacks flowing during both files.
    }
    assert.deepEqual(vad.finish(), [], 'EOF is idempotent');
    results.push(events);
  }
  Atomics.store(progress, lane + 2, 1);
  return results;
}

/** @param {number} value */
function nonnegativeInteger(value) {
  assert.ok(Number.isSafeInteger(value) && value >= 0);
}

/** @param {AttachedVadEvent} event */
function wellFormed(event) {
  assert.ok(event !== null && typeof event === 'object');
  nonnegativeInteger(event.epoch);
  nonnegativeInteger(event.seq);
  const fields = ['epoch', 'seq', 'type'];
  switch (event.type) {
    case 'epochStart':
      fields.push('captureSample', 'ptsNs');
      assert.equal(event.seq, 0);
      assert.equal(typeof event.captureSample, 'bigint');
      assert.ok(event.captureSample >= 0n);
      nonnegativeInteger(event.ptsNs);
      break;
    case 'segment':
    case 'provisionalCut':
      fields.push('startMs', 'endMs');
      nonnegativeInteger(event.startMs);
      nonnegativeInteger(event.endMs);
      assert.ok(event.endMs > event.startMs);
      if (event.type === 'segment') {
        assert.equal(event.startMs % 10, 0);
        assert.equal(event.endMs % 10, 0);
      } else {
        fields.push('reason');
        assert.ok(['limit', 'hysteresis', 'finish', 'reset'].includes(event.reason));
      }
      break;
    case 'provisionalSpeechStart':
    case 'provisionalSpeechEnd':
      fields.push('atMs');
      nonnegativeInteger(event.atMs);
      if (event.type === 'provisionalSpeechEnd') {
        fields.push('reason');
        assert.ok(['hysteresis', 'finish', 'reset'].includes(event.reason));
      }
      break;
    case 'epochEnd':
      fields.push('reason');
      assert.equal(event.reason, 'finish', 'healthy sessions close successfully');
      break;
    default:
      assert.fail('unexpected VAD event type');
  }
  assert.deepEqual(Object.keys(event).sort(), fields.sort());
}

async function waitFor(predicate, message) {
  const deadline = Date.now() + 10000;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, message);
    await delay(5);
  }
}

if (!isMainThread) {
  assert.ok(parentPort);
  assert.ok(workerData.progress instanceof SharedArrayBuffer);
  const progress = new Int32Array(workerData.progress);
  const files = speechFiles();
  parentPort.postMessage('ready');
  const barrier = Atomics.wait(progress, 4, 0, 10000);
  assert.ok(barrier === 'ok' || barrier === 'not-equal', 'main thread must release the start barrier');
  const results = await runFiles(files, true, progress, 1);
  parentPort.postMessage(results);
} else {
  const { test } = await import('node:test');
  test('concurrent standalone owners preserve isolated file events while attached capture stays healthy',
    { timeout: 60000 }, async t => {
      assert.equal(typeof Vad, 'function');
      assert.equal(Object.hasOwn(releaseNative, '__openMockStream'), false, 'standalone uses the plain release addon');
      const files = speechFiles();
      const reference = await runFiles(files, false, new Int32Array(new SharedArrayBuffer(20)), 0);
      const control = await runFiles(files, false, new Int32Array(new SharedArrayBuffer(20)), 0,
        { ...options, threshold: 1 });
      // This control precedes all oracle equalities and proves comparisons are sensitive.
      assert.notDeepEqual(control, reference, 'different thresholds must change the speech events');
      for (const events of reference) {
        assert.ok(events.filter(event => event.type === 'segment').length >= 3, 'reference must contain several segments');
      }
      t.diagnostic('Threshold control changed events; both reference files contain at least three segments.');

      const progress = new Int32Array(new SharedArrayBuffer(20));
      const worker = new Worker(new URL(import.meta.url), { workerData: { progress: progress.buffer } });
      t.after(async () => { await worker.terminate(); });
      let ready = false;
      /** @type {VadEvent[][] | undefined} */
      let workerResults;
      const completion = new Promise((resolve, reject) => {
        worker.on('message', message => {
          if (message === 'ready') ready = true;
          else workerResults = message;
        });
        worker.once('error', reject);
        worker.once('exit', code => {
          if (code !== 0) reject(new Error(`Worker exited with code ${code}`));
          else if (!workerResults) reject(new Error('Worker exited without file events'));
          else resolve(workerResults);
        });
      });
      // Observe early failures immediately, even while waiting for capture/startup.
      completion.catch(() => {});
      await waitFor(() => ready, 'Worker must reach the start barrier');
      /** @type {AudioChunk[]} */
      const chunks = [];
      /** @type {import('../index.js').JsStreamEvent[]} */
      const notifications = [];
      let overlappingChunks = 0;
      const stream = native.__openMockStream(48000, 2, 220, chunk => {
        chunks.push(chunk);
        if (chunk.frames > 0 && Atomics.load(progress, 0) > 0 && Atomics.load(progress, 1) > 0
          && Atomics.load(progress, 2) === 0 && Atomics.load(progress, 3) === 0) overlappingChunks++;
      }, undefined, undefined, undefined,
      { params: { threshold: 0, minSpeechMs: 0, speechPadMs: 0 }, provisional: true, tap: 'primary' },
      event => notifications.push(event));
      t.after(async () => { await stream.stop(); });
      await waitFor(() => chunks.some(chunk => chunk.frames > 0), 'mock capture must deliver before concurrent inference');
      Atomics.store(progress, 4, 1);
      Atomics.notify(progress, 4);
      const [mainResults, concurrentWorkerResults] = await Promise.all([
        runFiles(files, true, progress, 0), completion,
      ]);
      assert.ok(overlappingChunks >= 3, 'capture must deliver while both standalone owners are processing');

      const beforeFlush = chunks.flatMap(chunk => chunk.vadEvents ?? []);
      assert.ok(beforeFlush.some(event => event.type === 'provisionalSpeechStart'));
      const activeEpoch = beforeFlush.at(-1).epoch;
      // The mock PCM is deterministic, but elapsed-time PTS and the prefix consumed
      // before flush depend on scheduling. Independent live runs cannot share an EOF oracle.
      await stream.flushVad();
      assert.ok(chunks.flatMap(chunk => chunk.vadEvents ?? []).some(event =>
        event.type === 'epochEnd' && event.epoch === activeEpoch && event.reason === 'finish'),
      'flush must deliver closure before resolving');
      await waitFor(() => chunks.flatMap(chunk => chunk.vadEvents ?? []).some(event =>
        event.type === 'epochStart' && event.epoch > activeEpoch), 'capture must restart the attached epoch after flush');
      await stream.stop();
      assert.equal(notifications.some(event => event.type === 'vadFailed'), false);
      assert.equal(stream.terminalError(), undefined);
      assert.deepEqual(stream.shutdownReport(), { primary: null, cleanupErrors: [] });

      let epoch = -1;
      let seq = 0;
      let origin = -1n;
      let ended = true;
      let lastSegmentEnd = 0n;
      let segmentCount = 0;
      for (const chunk of chunks) {
        assert.ok(Array.isArray(chunk.vadEvents), 'attached event array must be present');
        for (const event of chunk.vadEvents) {
          wellFormed(event);
          if (event.type === 'epochStart') {
            assert.ok(ended, 'previous epoch must close before the next origin');
            assert.ok(event.epoch > epoch);
            assert.ok(event.captureSample > origin, 'capture origins must strictly advance');
            epoch = event.epoch;
            origin = event.captureSample;
            seq = 0;
            ended = false;
            lastSegmentEnd = origin;
          }
          assert.equal(ended, false, 'no payload outside an open epoch');
          assert.equal(event.epoch, epoch);
          assert.equal(event.seq, seq++);
          if (event.type === 'segment') {
            const start = origin + BigInt(event.startMs) * 48n;
            const end = origin + BigInt(event.endMs) * 48n;
            assert.ok(start >= lastSegmentEnd && end > start, 'segments must be ordered in capture samples');
            lastSegmentEnd = end;
            segmentCount++;
          }
          if (event.type === 'epochEnd') ended = true;
        }
      }
      assert.ok(ended, 'stop must close the last attached epoch');
      assert.ok(segmentCount >= 2, 'both sides of flush must produce attached segments');

      for (const results of [mainResults, concurrentWorkerResults]) {
        assert.ok(Array.isArray(results));
        assert.equal(results.length, files.length);
        for (const [file, events] of results.entries()) {
          assert.ok(Array.isArray(events));
          events.forEach((event, index) => {
            wellFormed(event);
            assert.equal(event.epoch, file);
            assert.equal(event.seq, index);
          });
          assert.equal(events.at(-1).type, 'epochEnd');
          assert.deepEqual(events, reference[file], 'concurrent file events must equal the isolated reference');
        }
      }
    });
}
