// Browser smoke test of @zen/client, driven by smoke.spec.ts: the page is
// served by zen-serve itself (unencrypted_dir), so the client's origin is the
// server's. The steps' results go to `window.smoke`.
import { browserAuthenticator, bytes, connect, prfSlotSalts } from './client/index.js';

const log = (m) => {
  document.getElementById('log').textContent += `${m}\n`;
};

async function run(cfg) {
  const out = {};
  const client = await connect(location.origin);
  out.origin = client.origin;

  // Password sign-in (method 6): Argon2id in WASM.
  const t0 = performance.now();
  const s = await client.signInPassword(cfg.name, cfg.password);
  out.passwordMs = Math.round(performance.now() - t0);
  log(`password sign-in in ${out.passwordMs} ms`);

  // Register a passkey with the PRF extension, and give it a keyslot.
  const auth = browserAuthenticator();
  const pk = await s.registerPasskey(auth, { prf: true, userName: cfg.name });
  out.prf = !!pk.prf;
  const admin = await s.fs(1).unlock({ passphrase: cfg.passphrase });
  await admin.addPasskey(pk.id, pk.prf);
  admin.close();
  log('passkey registered, keyslot added');

  // One touch: sign in and unlock with the PRF output.
  const h = (await s.fs(1).header()).header;
  const salts = new Map();
  for (const { credentialId, prfSalt } of prfSlotSalts(h)) {
    if (bytes.equal(credentialId, pk.id)) salts.set(bytes.b64url(pk.rawId), prfSalt);
  }
  const sp = await client.signInPasskey(auth, { user: s.userFp, prfSalts: salts });
  out.passkeyMethod = sp.info.method;
  const fs = await sp.fs(1).unlock();
  log('signed in by passkey, unlocked by PRF');

  // KV.
  await fs.kv.set(['smoke', 'hello'], bytes.utf8('from the browser'));
  out.kv = bytes.fromUtf8(await fs.kv.get(['smoke', 'hello']));

  if (cfg.extra) Object.assign(out, await cfg.extra(sp, fs));
  fs.close();
  return out;
}

window.runSmoke = async (cfg) => {
  try {
    return { ok: true, ...(await run(cfg)) };
  } catch (e) {
    return { ok: false, error: `${e?.name}: ${e?.message}\n${e?.stack}` };
  }
};
window.smokeReady = true;
