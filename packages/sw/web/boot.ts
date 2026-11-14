// The bootstrap page (spec/sw.md §1): the server answers a navigation on
// the content origin with it until the worker is installed. It registers
// the worker and reloads the same URL once the page is controlled.
import { registerWorker, say } from './trusted.js';

/** The URL this tab last reloaded for, and when: a worker that doesn't answer can't loop us. */
const RELOADED = 'zen-boot-reloaded';
const LOOP_MS = 10_000;

function reload(): void {
  let looping = false;
  try {
    const [url, at] = (sessionStorage.getItem(RELOADED) ?? '').split(' ');
    looping = url === location.href && Date.now() - Number(at) < LOOP_MS;
    if (looping) sessionStorage.removeItem(RELOADED);
    else sessionStorage.setItem(RELOADED, `${location.href} ${Date.now()}`);
  } catch {
    // No storage: reload anyway.
  }
  if (looping) say('status', 'The viewer is installed but did not answer. Reload to try again.');
  else location.reload();
}

async function boot(): Promise<void> {
  const sw = navigator.serviceWorker;
  // Opened at its own address, the worker passes it to the network: nothing to reload.
  const here = location.pathname.startsWith('/_zen/');
  if (sw.controller) {
    if (!here) reload();
    return;
  }
  sw.addEventListener('controllerchange', () => {
    if (!here) reload();
    else say('status', 'The viewer is installed.');
  });
  await registerWorker();
}

boot().catch((e: unknown) => say('status', `The viewer could not start: ${String(e)}`));
