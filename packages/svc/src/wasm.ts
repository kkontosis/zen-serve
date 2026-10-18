// @zen/svc-wasm, the protocol engines (TLS, SSH), loaded once and only by
// services whose layer needs it.
import type * as SW from '@zen/svc-wasm';

/** The module's exports. */
export type SvcWasm = typeof SW;

/** Where the module is, if not next to its glue (browsers) or in node_modules (Node). */
export type WasmSource = URL | string | Response | BufferSource;

let ready: Promise<SvcWasm> | undefined;

/**
 * Load and instantiate @zen/svc-wasm once. It is imported only here, so
 * services without `tls()` or `ssh()` never load it.
 */
export function loadSvcWasm(source?: WasmSource): Promise<SvcWasm> {
  ready ??= load(source).catch((e) => {
    ready = undefined;
    throw e;
  });
  return ready;
}

async function load(source?: WasmSource): Promise<SvcWasm> {
  const m = await import('@zen/svc-wasm');
  const isNode = typeof process !== 'undefined' && !!process.versions?.node;
  if (source === undefined && isNode) {
    const { readFile } = await import('node:fs/promises');
    const { createRequire } = await import('node:module');
    const path = createRequire(import.meta.url).resolve('@zen/svc-wasm/zen_svc_wasm_bg.wasm');
    m.initSync({ module: await readFile(path) });
  } else {
    await m.default(source === undefined ? undefined : { module_or_path: source });
  }
  return m;
}
