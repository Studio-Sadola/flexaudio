import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';
const require = createRequire(import.meta.url);
export const native = require(process.env.FLEXAUDIO_TEST_ADDON || './flexaudio.node');
export const releaseNative = process.env.FLEXAUDIO_RELEASE_ADDON
  ? require(process.env.FLEXAUDIO_RELEASE_ADDON)
  : existsSync(new URL('./plain-debug.node', import.meta.url)) ? require('./plain-debug.node') : native;
if (typeof native.__openMockStream !== 'function') {
  throw new Error('Build the test addon with the test-faults feature before running mock contracts');
}
