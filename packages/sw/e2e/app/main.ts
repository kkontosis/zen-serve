// The test app (packages/sw/e2e): an app-origin page that enrolls the
// content origin's worker with `enrollViewer` (spec/sw.md §8). The test
// passes the member's seeds through `page.evaluate`, never in a URL.
import { bytes, connect, initWasm, zw } from '@zen/client';
import { enrollViewer } from '@zen/sw';

export interface EnrollArgs {
  identitySeed: number[];
  deviceSeed: number[];
  /** When the member's certificate was issued (as the ACL holds it). */
  certUnix: number;
  contentOrigin: string;
  mounts: { name: string; fs: number; tree: string; base: string; listing?: boolean }[];
  admin: boolean;
}

async function enroll(a: EnrollArgs): Promise<{ fp: string; inAcl: boolean }> {
  await initWasm();
  const client = await connect(location.origin);
  const identity = zw.SigningIdentity.fromSeed(new Uint8Array(a.identitySeed));
  const device = zw.DeviceSecret.fromSeed(new Uint8Array(a.deviceSeed));
  const cert = identity.issueDeviceCert(device.devicePublic, BigInt(a.certUnix));
  const session = await client.signInDevice({ identity: identity.publicIdentity, device, cert });
  const ufs = await session.fs(1).unlock({ device });
  try {
    const r = await enrollViewer({
      session,
      identity,
      ...(a.admin ? { admin: identity } : {}),
      fs: [ufs],
      mounts: a.mounts.map((m) => ({ ...m, tree: bytes.fromHex(m.tree) })),
      contentOrigin: a.contentOrigin,
    });
    return { fp: bytes.hex(r.fp), inAcl: r.inAcl };
  } finally {
    ufs.close();
  }
}

(globalThis as { zenEnroll?: typeof enroll }).zenEnroll = enroll;
