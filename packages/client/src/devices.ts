// Device keys for checkpoint signers (spec/api.md §15.2, formats.md §7).
import type { DeviceSecret } from '@zen/wasm';
import { randomBytes } from './bytes.js';
import { zw } from './wasm.js';

/** A fresh device key. */
export interface NewDevice {
  secret: DeviceSecret;
  /** Its 32-byte seed (for the vault); zero it once kept. */
  seed: Uint8Array;
  /** The encoded public key a user certifies (`session.approveDevice`). */
  devicePublic: Uint8Array;
  /** Its fingerprint. */
  fp: Uint8Array;
}

/** Make a device key from 32 random bytes. */
export function newDevice(): NewDevice {
  const seed = randomBytes(32);
  const secret = zw.DeviceSecret.fromSeed(seed);
  const devicePublic = secret.devicePublic;
  return { secret, seed, devicePublic, fp: zw.fingerprint(devicePublic) };
}
