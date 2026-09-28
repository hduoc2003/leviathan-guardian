// Must precede the SDK import: the SDK's bundled dexie captures
// `globalThis.indexedDB` at module load.
import '../src/testing/fake-indexeddb-device.js';

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';

// The MT build inlines wasm-bindgen-rayon's worker snippet, which calls
// `self.addEventListener` at import time; a static import would be hoisted above this.
if (!('self' in globalThis)) {
  Object.defineProperty(globalThis, 'self', { value: new EventTarget(), configurable: true });
}
const { initSync } = await import('@miden-sdk/miden-sdk/lazy');

const require = createRequire(import.meta.url);
const sdkRootDir = dirname(require.resolve('@miden-sdk/miden-sdk/package.json'));
initSync({
  module: readFileSync(join(sdkRootDir, 'dist', 'mt', 'assets', 'miden_client_web.wasm')),
});
