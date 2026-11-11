import test from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';
import { native } from './load-addon.mjs';

async function waitForLevel(stream) {
  const deadline = Date.now() + 3000;
  for (;;) {
    const value = stream.pollLevel();
    if (value !== undefined) return value;
    assert.ok(Date.now() < deadline, 'input must refresh the level without PCM polling');
    await delay(5);
  }
}

test('paused shared levels persist across reads without new PCM or attached VAD', async () => {
  const chunks = [];
  const stream = native.__openMockStream(48000, 2, 440, chunk => chunks.push(chunk),
    16000, 1, 'f32', { tap: 'primary', params: {} });
  try {
    stream.setGain(0.5);
    await waitForLevel(stream);
    stream.pause();
    await delay(150); // Allow pre-pause queues to drain under the existing contract.
    assert.equal(stream.isPaused(), true);
    await delay(25); // Drain callbacks queued before the bridge acknowledged pause.
    const count = chunks.length;
    const a = await waitForLevel(stream);
    assert.ok(a.peak > 0.45 && a.peak <= 0.5,
      'input levels must be measured before gain and primary output conversion');
    assert.ok(a.rms > 0 && a.rms <= a.peak);
    assert.equal(typeof a.ageNs, 'bigint');
    // Calls are synchronous direct reads; they cannot consume the cell.
    const started = performance.now();
    for (let i = 0; i < 2000; i++) assert.notEqual(stream.pollLevel(), undefined);
    assert.ok(performance.now() - started < 500,
      'level reads must not wait for the bridge polling tick on every call');
    await delay(100);
    assert.equal(chunks.length, count, 'paused input must produce neither PCM nor VAD carriers');
    stream.setGain(0);
    stream.pause(); // Clear the cell, requiring a new measurement while muted.
    const muted = await waitForLevel(stream);
    assert.ok(muted.peak > 0.45 && muted.rms > 0, 'mute must not silence the input meter');
    stream.resume();
    await delay(100);
    assert.ok(chunks.length > count, 'recording must resume');
  } finally {
    const stopped = stream.stop();
    assert.equal(stream.pollLevel(), undefined, 'stop request closes the shared meter immediately');
    await stopped;
  }
  assert.equal(stream.pollLevel(), undefined);
});

test('silent input meters zero and source replacement still updates the shared cell', async () => {
  const stream = native.openStream({ kind: 'mock', gain: 0 }, () => {});
  try {
    const level = await waitForLevel(stream);
    assert.equal(level.peak, 0);
    assert.equal(level.rms, 0);
    stream.pause();
    stream.switchSource({ kind: 'mock' });
    const next = await waitForLevel(stream);
    assert.equal(next.peak, 0);
    assert.equal(next.rms, 0);
  } finally { await stream.stop(); }
});

test('terminal level reads throw the retained typed error even after stop', async () => {
  const stream = native.__bindingContractBridge('attachment-flush-primary', () => {});
  await assert.rejects(stream.stop(), { code: 'FLEX_UNSUPPORTED_FORMAT' });
  assert.throws(() => stream.pollLevel(), { code: 'FLEX_UNSUPPORTED_FORMAT' });
});
