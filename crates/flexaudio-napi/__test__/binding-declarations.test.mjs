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
});


test('release declarations and loader exclude test controls', () => {
  const loader = readFileSync(new URL('../index.js', import.meta.url), 'utf8');
  for (const source of [declarations, loader]) {
    assert.ok(!source.includes('__openMockStream'));
    assert.ok(!source.includes('__vadTestFaults'));
    assert.ok(!source.includes('TestFaultsUnavailable'));
    assert.ok(!source.includes('__bindingContractCapabilities'));
    assert.ok(!source.includes('__bindingContractOpenOptions'));
  }
});

test('capabilities declare the complete snapshot and closed availability payloads', () => {
  assert.match(declarations, /export declare function capabilities\(\): Promise<Capabilities>/);
  for (const [field, type] of Object.entries({ osVersion: 'OsVersion | null',
    sources: 'SourceCapabilities', systemExclusions: 'SystemExclusionCapabilities',
    process: 'ProcessCapabilities', deviceWatcher: 'DeviceWatcherCapabilities',
    sideEffects: 'SideEffectCapabilities' })) {
    assert.ok(body('Capabilities').includes(`${field}: ${type}`));
  }
  assert.deepEqual([...alias('CapabilityAvailability').matchAll(/status: '([^']+)'/g)].map(match => match[1]),
    ['supported', 'unavailable', 'unknown']);
  assert.ok(!alias('CapabilityAvailability').includes('status: string'));
  assert.deepEqual([...alias('CapabilityUnavailableReason').matchAll(/kind: '([^']+)'/g)].map(match => match[1]),
    ['unsupportedOs', 'osVersionTooOld', 'pipeWireUnavailable', 'notApplicable', 'other']);
  assert.match(alias('CapabilityAvailability'), /status: 'unavailable'; reason: CapabilityUnavailableReason/);
  assert.match(alias('CapabilityAvailability'), /status: 'unknown'; error: AudioError/);
  assert.match(alias('CapabilityUnavailableReason'), /kind: 'other'; code: string/);
  assert.match(alias('CapabilityUnavailableReason'), /kind: 'osVersionTooOld'; required: OsVersion; actual: OsVersion/);
  for (const [name, tags] of Object.entries({
    BackendKind: ['cpal', 'pipeWire', 'wasapiLoopback', 'wasapiProcessLoopback', 'coreAudioProcessTap', 'composite', 'unsupported', 'other'],
    PidScope: ['exactPid', 'processTree', 'notApplicable', 'other'],
    DeviceSelectionBehavior: ['honored', 'ignored', 'notApplicable', 'other'],
    WatcherMode: ['native', 'noop', 'other'], WatchScope: ['systemOutputs', 'defaultSystemOutput', 'other'],
    SideEffectCondition: ['silentActiveSystemAudio', 'classicLoopbackWithoutExclusions', 'other'],
  })) {
    assert.deepEqual([...alias(name).matchAll(/'([^']+)'/g)].map(match => match[1]), tags);
  }
  assert.match(body('OsVersion'), /patch: number \| null/);
  assert.match(body('OsVersion'), /build: number \| null/);
  assert.match(body('SystemExclusionCapabilities'), /maxDistinctPids: number \| null/);
  assert.match(body('SourceCapabilities'), /mic: SourceCapability\s+system: SourceCapability\s+process: SourceCapability\s+mix: SourceCapability/);
  assert.match(body('SideEffectCapability'), /sources: Array<'mic' \| 'system' \| 'process' \| 'mix' \| 'other'>/);
  const loader = readFileSync(new URL('../index.js', import.meta.url), 'utf8');
  assert.match(loader, /module\.exports\.capabilities = capabilities/);
});

test('keepalive is an optional boolean with distinct open and switch defaults', () => {
  assert.match(body('OpenOptions'), /windowsClassicLoopbackKeepalive\?: boolean/);
  assert.match(declarations, /Omitted on openStream defaults to true; omitted on switchSource keeps the current value/);
  assert.match(declarations, /options\.windowsClassicLoopbackKeepalive.*replaces the current value/);
});
