// Loading the WASM core (@zen/wasm). Every entry point that needs it awaits
// `initWasm()` first; `connect()` does.
import * as zw from '@zen/wasm';

export { zw };

let ready: Promise<void> | undefined;

/**
 * Load and instantiate the WASM module once. In Node it is read from the
 * @zen/wasm package; in a browser it is fetched next to the glue script,
 * unless `source` (a URL, a Response or the module bytes) says otherwise.
 */
export function initWasm(source?: URL | string | Response | BufferSource): Promise<void> {
  ready ??= load(source);
  return ready;
}

async function load(source?: URL | string | Response | BufferSource): Promise<void> {
  const isNode = typeof process !== 'undefined' && !!process.versions?.node;
  if (source === undefined && isNode) {
    const { readFile } = await import('node:fs/promises');
    const { createRequire } = await import('node:module');
    const path = createRequire(import.meta.url).resolve('@zen/wasm/zen_wasm_bg.wasm');
    zw.initSync({ module: await readFile(path) });
    return;
  }
  await zw.default(source === undefined ? undefined : { module_or_path: source });
}
