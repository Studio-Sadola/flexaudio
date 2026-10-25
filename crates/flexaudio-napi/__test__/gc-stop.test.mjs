import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

for (const mode of ['implicit', 'implicit-throwing', 'explicit', 'encoder', 'encoder-explicit']) {
  test(`GC stop reports a warning once only after ${mode} shutdown`,
    { skip: mode.startsWith('encoder') && process.platform !== 'linux' && 'the encoder failure fixture requires Linux /proc/self/fd pipes' }, () => {
    const result = spawnSync(process.execPath,
      ['--expose-gc', fileURLToPath(new URL('./gc-stop.mjs', import.meta.url)), mode],
      { encoding: 'utf8', timeout: 10000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    // The child must reach its end: the warning never killed the process.
    assert.match(result.stdout, /gc-stop: ok/, result.stdout + result.stderr);
  });
}
