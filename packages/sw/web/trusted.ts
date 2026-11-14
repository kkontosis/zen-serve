// The trusted pages' one Trusted Types sink: registering the worker
// (the server's policy has `require-trusted-types-for 'script'`).
const SW = '/_zen/sw.js';

interface TrustedTypesLike {
  createPolicy(
    name: string,
    rules: { createScriptURL(s: string): string },
  ): { createScriptURL(s: string): unknown };
}

/** Register the worker (sw.md §1) and resolve once it is active. */
export async function registerWorker(): Promise<ServiceWorkerRegistration> {
  const tt = (globalThis as { trustedTypes?: TrustedTypesLike }).trustedTypes;
  const url = tt
    ? tt
        .createPolicy('zen-sw', {
          createScriptURL: (s) => {
            if (s !== SW) throw new TypeError(`not the worker: ${s}`);
            return s;
          },
        })
        .createScriptURL(SW)
    : SW;
  await navigator.serviceWorker.register(url as string, { type: 'module', scope: '/' });
  return navigator.serviceWorker.ready;
}

/** Show `text` in the element with id `id`. */
export function say(id: string, text: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = text;
}
