import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const declarations = readFileSync(new URL('../index.d.ts', import.meta.url), 'utf8');
const red = { skip: process.env.FLEXAUDIO_RUN_REPRO === '1' ? false : 'repro: D M9' };
function body(name) {
  return declarations.split(`export interface ${name} {`)[1].split('}')[0];
}
test('repro_p9_stream_event_union', red, () => {
  assert.ok(!body('JsStreamEvent').includes('type: string'), 'JsStreamEvent.type accepts arbitrary strings');
});
test('repro_p9_device_event_union', red, () => {
  assert.ok(!body('JsDeviceEvent').includes('type: string'), 'JsDeviceEvent.type accepts arbitrary strings');
});
test('repro_p9_event_union_control', () => {
  assert.ok(!body('JsVadEvent').includes('type: string'));
  assert.match(body('JsVadEvent'), /type: 'speechStart' \| 'speechEnd'/);
});
