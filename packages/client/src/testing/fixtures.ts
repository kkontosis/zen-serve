// Test identities: a user, a device and its certificate, from seeds.
import type { DeviceSecret, SigningIdentity } from '@zen/wasm';
import { zw } from '../wasm.js';

/** A user with one device. */
export interface TestUser {
  identity: SigningIdentity;
  device: DeviceSecret;
  cert: Uint8Array;
  fp: Uint8Array;
}

/** A user whose identity and device derive from a one-byte seed (needs `initWasm`). */
export function testUser(seed: number): TestUser {
  const identity = zw.SigningIdentity.fromSeed(new Uint8Array(32).fill(seed));
  const device = zw.DeviceSecret.fromSeed(new Uint8Array(32).fill(seed ^ 0x80));
  const cert = identity.issueDeviceCert(device.devicePublic, 1_700_000_000n);
  return { identity, device, cert, fp: identity.fingerprint };
}
