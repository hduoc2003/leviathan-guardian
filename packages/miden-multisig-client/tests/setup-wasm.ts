import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';

import { initSync } from '@miden-sdk/miden-sdk';

const require = createRequire(import.meta.url);
// The fork's `exports` declares only "." - see `setup-globals.ts`.
const sdkDistDir = dirname(require.resolve('@miden-sdk/miden-sdk'));
// The fork keeps the WASM asset in dist/assets and exposes `initSync` from its
// single entry. Public 0.15.x added a "./lazy" subpath and moved the asset to
// dist/st/assets; neither applies here.
initSync({
  module: readFileSync(join(sdkDistDir, 'assets', 'miden_client_web.wasm')),
});
