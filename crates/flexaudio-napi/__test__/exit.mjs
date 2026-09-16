// After await stream.stop(), Node must exit by itself even if the stream
// handle is still referenced. Do not call process.exit.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const native = require(join(here, 'flexaudio.node'));

const stream = native.__openMockStream(48000, 2, 440.0, () => {});
await new Promise((r) => setTimeout(r, 80));
await stream.stop();
console.log('EXIT OK');
