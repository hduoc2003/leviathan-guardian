import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';

// The Leviathan SDK ships a multi-threaded WASM build aimed at browsers. At
// module scope its wasm-bindgen-rayon helper reads `self` and registers a
// message listener, and the loader fetches the .wasm asset over HTTP. Node has
// none of that, and ESM imports are hoisted, so these globals must exist before
// the SDK module is evaluated - hence a separate setup file ordered ahead of
// `setup-wasm.ts` in `vitest.config.ts`.
const scope = globalThis as Record<string, unknown>;

// Worker bootstrap only. It never receives a message on the main thread and the
// thread pool is never started, so no-ops are enough.
for (const method of ['addEventListener', 'removeEventListener', 'postMessage']) {
  if (typeof scope[method] !== 'function') {
    scope[method] = () => {};
  }
}

scope.self ??= globalThis;

const require = createRequire(import.meta.url);
// The fork's `exports` only declares ".", so `./package.json` is not
// resolvable; derive the asset dir from the main entry (<root>/dist/index.js).
const sdkDistDir = dirname(require.resolve('@miden-sdk/miden-sdk'));
const wasmBytes = readFileSync(join(sdkDistDir, 'assets', 'miden_client_web.wasm'));

// Serve the packaged .wasm from disk instead of the network. Anything else is
// left to the real fetch so an accidental outbound call still fails loudly.
const realFetch = scope.fetch as typeof fetch | undefined;
scope.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
  if (url.endsWith('.wasm')) {
    return new Response(wasmBytes, {
      status: 200,
      headers: { 'content-type': 'application/wasm' },
    });
  }
  if (!realFetch) {
    throw new Error(`no fetch available for ${url}`);
  }
  return realFetch(input, init);
}) as typeof fetch;
