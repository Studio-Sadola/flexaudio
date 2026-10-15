import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const declarations = readFileSync(new URL('../index.d.ts', import.meta.url), 'utf8');
function alias(name) {
  const start = declarations.indexOf(`export type ${name} =`);
  assert.ok(start >= 0, `${name} must be an exported type alias`);
  const end = declarations.indexOf('\nexport ', start + 1);
  return declarations.slice(start, end < 0 ? undefined : end);
}
function body(name) {
  return declarations.split(`export interface ${name} {`)[1].split('}')[0];
}
function checkUnion(name, arms) {
  const text = alias(name);
  assert.ok(!text.includes('type: string'), `${name}.type accepts arbitrary strings`);
  assert.deepEqual([...text.matchAll(/type: '([^']+)'/g)].map(match => match[1]).sort(), Object.keys(arms).sort());
  for (const [tag, payload] of Object.entries(arms)) {
    const arm = text.match(new RegExp(`\\{ type: '${tag}'([^}]*?)\\}`))?.[1];
    assert.notEqual(arm, undefined, `missing ${tag} arm`);
    assert.equal(arm.trim(), payload, `required payload for ${tag}`);
    assert.ok(!arm.includes('?:'), `optional required payload for ${tag}`);
  }
}
test('binding_contract_stream_event_union', () => {
  checkUnion('JsStreamEvent', {
    chunkDropped: '; count: bigint', stalled: '', recovered: '',
    permissionDenied: "; permission: 'microphone' | 'systemAudio'; message: string",
    permissionPending: "; permission: 'microphone' | 'systemAudio'; message: string",
    silenceWhileSourceActive: '; message: string', deviceLost: '', error: '; message: string',
    terminalError: '; error: AudioError', recoverableError: '; error: AudioError',
    shutdownError: '; error: AudioError',
    vadFailed: '; error: AudioError; epoch: number',
    audioLoss: '; loss: AudioLoss', clipped: '',
    startupSamplesDropped: "; frames: bigint; reason: 'outOfRange'",
    unknown: '; message: string',
  });
});
test('binding_contract_device_event_union', () => {
  checkUnion('JsDeviceEvent', {
    added: '; device: JsDeviceInfo', removed: '; id: string',
    defaultChanged: "; sourceKind: 'mic' | 'system'; id: string",
    defaultCleared: "; sourceKind: 'mic' | 'system'", rescanRequired: '; droppedEvents: bigint',
    terminalError: '; error: AudioError', unknown: '; message: string',
  });
});
test('VAD events expose segment and capture-origin discriminants', () => {
  assert.match(alias('VadEvent'), /type: 'segment'; startMs: number; endMs: number/);
  assert.match(alias('AttachedVadEvent'), /type: 'epochStart'; epoch: number; seq: 0; captureSample: bigint; ptsNs: number/);
});
test('audio errors have closed kind-specific payloads and exact loss counts', () => {
  const error = alias('AudioError');
  assert.match(error, /contexts: readonly ErrorContext\[\]/);
  assert.match(error, /secondary: readonly AudioError\[\]/);
  assert.match(error, /kind: 'permissionDenied'; permission: 'microphone' \| 'systemAudio'/);
  assert.match(error, /kind: 'nativeFormatChanged'; advertised: JsNativeFormat; actual: JsNativeFormat/);
  assert.ok(!/kind: string|permission\?:|advertised\?:|actual\?:/.test(error));
  assert.match(body('AudioLoss'), /samples: bigint \| null/);
  assert.ok(!/\bany\b/.test(declarations.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '')));
});

test('retained addon shutdown failures declare their typed VAD code and audio error tree', () => {
  assert.match(alias('AudioError'), /kind: 'backend'; vadCode\?: VadErrorCode/);
  assert.match(declarations, /export interface VadError extends Error \{[^}]*audioError\?: AudioError/);
  assert.match(body('ShutdownReport'), /primary: AudioError \| null/);
  assert.match(body('ShutdownReport'), /cleanupErrors: readonly AudioError\[\]/);
});


test('release declarations and loader exclude test controls', () => {
  const loader = readFileSync(new URL('../index.js', import.meta.url), 'utf8');
  for (const source of [declarations, loader]) {
    assert.ok(!source.includes('__openMockStream'));
    assert.ok(!source.includes('__vadTestFaults'));
    assert.ok(!source.includes('TestFaultsUnavailable'));
  }
});
