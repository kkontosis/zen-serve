// The enrollment page (spec/sw.md §8): opened by an app as a popup, it
// makes this browser's device key, asks the user to confirm the app's
// offer, keeps the device and the mounts, and registers the worker.
import { indexedDbLocalStore } from '@zen/client';
import { enrollPage, type Offer } from '../src/index.js';
import { registerWorker, say } from './trusted.js';

const DB = 'zen-sw';

/** Wait for a click on the confirm or the decline button. */
function ask(offer: Offer): Promise<boolean> {
  say('origin', offer.origin);
  const list = document.getElementById('mounts')!;
  list.replaceChildren(
    ...offer.mounts.map((m) => {
      const li = document.createElement('li');
      li.textContent = `/${m.name}/ → fs ${m.fs}, ${m.base}${m.root ? ' (also /)' : ''}`;
      return li;
    }),
  );
  document.getElementById('offer')!.hidden = false;
  const yes = document.getElementById('confirm') as HTMLButtonElement;
  const no = document.getElementById('decline') as HTMLButtonElement;
  return new Promise((resolve) => {
    yes.addEventListener('click', () => resolve(true), { once: true });
    no.addEventListener('click', () => resolve(false), { once: true });
  });
}

async function main(): Promise<void> {
  const r = await fetch('/_zen/config.json', { cache: 'no-store' });
  const config = (await r.json()) as { app_origins?: unknown };
  const appOrigins = Array.isArray(config.app_origins)
    ? config.app_origins.filter((o): o is string => typeof o === 'string')
    : [];
  if (!window.opener) {
    say('status', 'Open this page from your app, to enroll this browser as a viewer.');
    return;
  }
  say('status', 'Waiting for the app…');
  const done = await enrollPage({
    win: window as never,
    opener: window.opener as Window,
    appOrigins,
    local: indexedDbLocalStore(DB),
    onDevice: (fp) => say('fp', fp),
    confirm: ask,
    register: async () => {
      const reg = await registerWorker();
      reg.active?.postMessage({ type: 'zen-restart' });
    },
  });
  document.getElementById('offer')!.hidden = true;
  if (done.saved) {
    say('status', 'This browser is enrolled.');
    window.close();
  } else {
    say('status', `Not enrolled: ${done.reason ?? 'unknown'}.`);
  }
}

main().catch((e: unknown) => say('status', `Enrollment failed: ${String(e)}`));
