import { createRequire } from 'node:module';
import { test as nodeTest } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
let native;
try { native = createRequire(import.meta.url)('./flexaudio.node'); }
catch (error) { if (error.code !== 'MODULE_NOT_FOUND') throw error; }
function test(name, options, fn) {
  if (typeof options === 'function') { fn = options; options = {}; }
  return nodeTest(name, native ? options : { skip: options.skip || 'native build blocked: libspa E0425 SPA_ID_INVALID' }, fn);
}
const runRed = process.env.FLEXAUDIO_RUN_REPRO === '1';
const red = id => ({ skip: runRed ? false : `repro: ${id}` });
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
  `, fileURLToPath(new URL('./flexaudio.node', import.meta.url))], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, `binding aborted instead of throwing: signal=${output.signal}\n${output.stderr}`);
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
  `, fileURLToPath(new URL('./flexaudio.node', import.meta.url))], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, output.stderr);
  assert.match(output.stdout, /observed=true/);
});

test('repro_p9_reaper_failure_control', () => {
  const output = spawnSync(process.execPath, ['-e', `
    const native = require(process.argv[1]);
    const stream = native.__reproP9Bridge('reaper', () => {});
    setTimeout(async () => { await stream.stop(); console.log('stop resolved despite reaper spawn failure'); process.exit(0); }, 100);
  `, fileURLToPath(new URL('./flexaudio.node', import.meta.url))], { encoding: 'utf8', timeout: 10000 });
  assert.equal(output.status, 0, output.stderr);
  assert.match(output.stdout, /stop resolved despite reaper spawn failure/);
});
