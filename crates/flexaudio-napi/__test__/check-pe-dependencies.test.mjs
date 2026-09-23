import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { isReadableDllName } from './pe-dll-name.mjs';

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

test('CLI rejects an invocation without input files', () => {
  const result = spawnSync(process.execPath, [CHECK_PE_DEPENDENCIES], { encoding: 'utf8', timeout: 10_000 });

  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /FAIL: no input file given/);
});
