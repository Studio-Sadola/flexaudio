import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { isReadableDllName } from './pe-dll-name.mjs';
import { buildPe } from './pe-fixture.mjs';
import { ALLOWED_DLL_NAMES, FORBIDDEN_PATTERNS, judgeDependencies } from './pe-dependency-policy.mjs';

const CHECK_PE_DEPENDENCIES = fileURLToPath(new URL('./check-pe-dependencies.mjs', import.meta.url));

const VALID_DLL_NAMES = [
  'ADVAPI32.dll',
  'api-ms-win-crt-convert-l1-1-0.dll',
  'api-ms-win-crt-environment-l1-1-0.dll',
  'api-ms-win-crt-filesystem-l1-1-0.dll',
  'api-ms-win-crt-heap-l1-1-0.dll',
  'api-ms-win-crt-locale-l1-1-0.dll',
  'api-ms-win-crt-math-l1-1-0.dll',
  'api-ms-win-crt-multibyte-l1-1-0.dll',
  'api-ms-win-crt-private-l1-1-0.dll',
  'api-ms-win-crt-runtime-l1-1-0.dll',
  'api-ms-win-crt-stdio-l1-1-0.dll',
  'api-ms-win-crt-string-l1-1-0.dll',
  'api-ms-win-crt-time-l1-1-0.dll',
  'api-ms-win-crt-utility-l1-1-0.dll',
  'cublas64_12.dll',
  'cudart64_12.dll',
  'ggml-base.dll',
  'ggml-cpu.dll',
  'ggml.dll',
  'KERNEL32.dll',
  'libc++.dll',
  'libomp.dll',
  'libunwind.dll',
  'libwhisper.dll',
  'MSVCP140.dll',
  'nvcuda.dll',
  'VCOMP140.DLL',
  'VCRUNTIME140_1.dll',
  'VCRUNTIME140.dll',
  'vulkan-1.dll',
  'whisper.dll',
];

const INVALID_DLL_NAMES = [
  '',
  'foo.exe',
  'foo.dll.bak',
  'a/b.dll',
  'a\\b.dll',
  'line\nbreak.dll',
  'tab\tname.dll',
  'café.dll',
  'fullwidth．dll',
  `${'a'.repeat(256)}.dll`,
];

const CURRENT_RELEASE_DLL_NAMES = [
  'kernel32.dll',
  'bcryptprimitives.dll',
  'api-ms-win-core-synch-l1-2-0.dll',
  'ole32.dll',
  'ntdll.dll',
  'mmdevapi.dll',
  'oleaut32.dll',
  'propsys.dll',
];

const PREVIOUS_RELEASE_DLL_NAMES = [
  'kernel32.dll',
  'api-ms-win-core-synch-l1-2-0.dll',
  'bcryptprimitives.dll',
  'dxgi.dll',
  'd3d12.dll',
  'DirectML.dll',
  'advapi32.dll',
  'dbghelp.dll',
  'api-ms-win-core-path-l1-1-0.dll',
  'setupapi.dll',
  'ole32.dll',
  'ntdll.dll',
  'mmdevapi.dll',
  'oleaut32.dll',
  'propsys.dll',
  'MSVCP140.dll',
  'MSVCP140_1.dll',
  'VCRUNTIME140.dll',
  'VCRUNTIME140_1.dll',
  'api-ms-win-crt-math-l1-1-0.dll',
  'api-ms-win-crt-string-l1-1-0.dll',
  'api-ms-win-crt-runtime-l1-1-0.dll',
  'api-ms-win-crt-heap-l1-1-0.dll',
  'api-ms-win-crt-stdio-l1-1-0.dll',
  'api-ms-win-crt-convert-l1-1-0.dll',
  'api-ms-win-crt-filesystem-l1-1-0.dll',
  'api-ms-win-crt-time-l1-1-0.dll',
  'api-ms-win-crt-locale-l1-1-0.dll',
];

test('accepts observed PE dependency DLL names', () => {
  assert.deepEqual(
    VALID_DLL_NAMES.filter((name) => !isReadableDllName(name)),
    [],
  );
});

test('rejects malformed or unsafe DLL names', () => {
  assert.deepEqual(
    INVALID_DLL_NAMES.filter((name) => isReadableDllName(name)),
    [],
  );
});

test('allows the current release DLL dependencies', () => {
  assert.deepEqual(judgeDependencies(CURRENT_RELEASE_DLL_NAMES), { forbidden: [], unexpected: [] });
});

test('matches allowed DLL names without regard to case', () => {
  assert.deepEqual(judgeDependencies(['KERNEL32.dll']), { forbidden: [], unexpected: [] });
});

test('allows API set DLL names by prefix', () => {
  assert.deepEqual(judgeDependencies(['api-ms-win-crt-heap-l1-1-0.dll']), { forbidden: [], unexpected: [] });
});

test('reports unallowlisted Windows DLL names as unexpected', () => {
  assert.deepEqual(judgeDependencies(['user32.dll']), { forbidden: [], unexpected: ['user32.dll'] });
});

test('reports forbidden names without duplicating them as unexpected', () => {
  assert.deepEqual(judgeDependencies(['VCRUNTIME140.dll']), {
    forbidden: [{ name: 'VCRUNTIME140.dll', pattern: 'vcruntime' }],
    unexpected: [],
  });
});

test('reports libc++ as unexpected', () => {
  assert.deepEqual(judgeDependencies(['libc++.dll']), { forbidden: [], unexpected: ['libc++.dll'] });
});

test('classifies every dependency in the previous release', () => {
  assert.deepEqual(judgeDependencies(PREVIOUS_RELEASE_DLL_NAMES), {
    forbidden: [
      { name: 'DirectML.dll', pattern: 'directml' },
      { name: 'MSVCP140.dll', pattern: 'msvcp' },
      { name: 'MSVCP140_1.dll', pattern: 'msvcp' },
      { name: 'VCRUNTIME140.dll', pattern: 'vcruntime' },
      { name: 'VCRUNTIME140_1.dll', pattern: 'vcruntime' },
    ],
    unexpected: ['dxgi.dll', 'd3d12.dll', 'advapi32.dll', 'dbghelp.dll', 'setupapi.dll'],
  });
});

test('does not allow DLL names matched by forbidden patterns', () => {
  assert.deepEqual(
    ALLOWED_DLL_NAMES.filter((name) => FORBIDDEN_PATTERNS.some((pattern) => name.includes(pattern))),
    [],
  );
});

test('stores every allowed DLL name in lowercase', () => {
  assert.deepEqual(
    ALLOWED_DLL_NAMES.filter((name) => name !== name.toLowerCase()),
    [],
  );
});

function runFixtureCli(spec) {
  const directory = mkdtempSync(join(tmpdir(), 'flexaudio-pe-'));
  const file = join(directory, 'fixture.node');
  try {
    writeFileSync(file, buildPe(spec));
    return spawnSync(process.execPath, [CHECK_PE_DEPENDENCIES, file], { encoding: 'utf8', timeout: 10_000 });
  } finally {
    rmSync(directory, { force: true, recursive: true });
  }
}

test('CLI allows a PE whose imports are all allowed', () => {
  const result = runFixtureCli({
    imports: ['kernel32.dll', 'mmdevapi.dll', 'api-ms-win-core-synch-l1-2-0.dll'],
  });

  assert.ifError(result.error);
  assert.equal(result.status, 0);
  assert.match(result.stdout, /     - mmdevapi\.dll/);
  assert.match(result.stdout, /OK: 1 file\(s\) inspected/);
});

test('CLI rejects an unexpected import from a PE', () => {
  const result = runFixtureCli({ imports: ['kernel32.dll', 'user32.dll'] });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /unexpected native dependency \(user32\.dll\)/);
});

test('CLI reads libc++ from a PE and rejects it as unexpected', () => {
  const result = runFixtureCli({ imports: ['kernel32.dll', 'libc++.dll'] });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /unexpected native dependency \(libc\+\+\.dll\)/);
  assert.doesNotMatch(result.stderr, /not a readable DLL name/);
});

test('CLI rejects a forbidden delay import from a PE', () => {
  const result = runFixtureCli({
    imports: ['kernel32.dll'],
    delayImports: ['VCRUNTIME140.dll'],
  });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /forbidden native dependency \(VCRUNTIME140\.dll/);
});

test('CLI rejects a PE with no DLL dependencies', () => {
  const result = runFixtureCli({ imports: [] });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /no DLL dependency could be read/);
});

test('CLI rejects an invocation without input files', () => {
  const result = spawnSync(process.execPath, [CHECK_PE_DEPENDENCIES], { encoding: 'utf8', timeout: 10_000 });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /FAIL: no input file given/);
});
