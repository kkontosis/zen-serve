// A software WebAuthn authenticator for Node and tests: ES256 keys, "none"
// attestation, a signature counter, discoverable credentials and the PRF
// extension (HMAC-SHA-256 under a per-credential secret, like CTAP2's
// hmac-secret). It keeps everything in memory.
import { b64url, concat, randomBytes, utf8 } from '../bytes.js';
import type {
  Authenticator,
  CreateRequest,
  CreateResult,
  GetRequest,
  GetResult,
} from '../passkey.js';
import { CborMap, cbor } from './cbor.js';

interface Cred {
  rawId: Uint8Array;
  rpId: string;
  userHandle: Uint8Array;
  key: CryptoKey;
  counter: number;
  prfSecret: Uint8Array;
}

const subtle = globalThis.crypto.subtle;

async function sha256(b: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await subtle.digest('SHA-256', b as BufferSource));
}

/** A raw `r ‖ s` ECDSA signature as DER (WebAuthn wants DER). */
function der(raw: Uint8Array): Uint8Array {
  const int = (x: Uint8Array) => {
    let i = 0;
    while (i < x.length - 1 && x[i] === 0) i++;
    let v: Uint8Array = x.slice(i);
    if (v[0]! & 0x80) v = concat(new Uint8Array([0]), v);
    return concat(new Uint8Array([0x02, v.length]), v);
  };
  const body = concat(int(raw.slice(0, 32)), int(raw.slice(32)));
  return concat(new Uint8Array([0x30, body.length]), body);
}

function u32(n: number): Uint8Array {
  return new Uint8Array([(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff]);
}

/** Options of the software authenticator. */
export interface SoftAuthenticatorOptions {
  /** Support the PRF extension (default true). */
  prf?: boolean;
  /** Report user verification (default true). */
  userVerified?: boolean;
}

export class SoftAuthenticator implements Authenticator {
  private readonly creds = new Map<string, Cred>();
  constructor(private readonly opts: SoftAuthenticatorOptions = {}) {}

  /** The WebAuthn ids of the credentials held. */
  get ids(): Uint8Array[] {
    return [...this.creds.values()].map((c) => c.rawId);
  }

  private flags(attested: boolean): number {
    return 0x01 | (this.opts.userVerified === false ? 0 : 0x04) | (attested ? 0x40 : 0);
  }

  private clientData(type: string, challenge: Uint8Array, origin: string): Uint8Array {
    return utf8(JSON.stringify({ type, challenge: b64url(challenge), origin, crossOrigin: false }));
  }

  private async prf(c: Cred, salt: Uint8Array | undefined): Promise<Uint8Array | undefined> {
    if (!salt || this.opts.prf === false) return undefined;
    const input = await sha256(concat(utf8('WebAuthn PRF'), new Uint8Array([0]), salt));
    const k = await subtle.importKey(
      'raw',
      c.prfSecret as BufferSource,
      { name: 'HMAC', hash: 'SHA-256' },
      false,
      ['sign'],
    );
    return new Uint8Array(await subtle.sign('HMAC', k, input as BufferSource));
  }

  async create(req: CreateRequest): Promise<CreateResult> {
    if (!req.algorithms.includes(-7)) throw new Error('soft authenticator: ES256 not offered');
    for (const id of req.exclude) {
      if (this.creds.has(b64url(id))) throw new Error('InvalidStateError: credential excluded');
    }
    const pair = (await subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, [
      'sign',
      'verify',
    ])) as CryptoKeyPair;
    const jwk = await subtle.exportKey('jwk', pair.publicKey);
    const x = Uint8Array.from(atob(jwk.x!.replace(/-/g, '+').replace(/_/g, '/')), (ch) =>
      ch.charCodeAt(0),
    );
    const y = Uint8Array.from(atob(jwk.y!.replace(/-/g, '+').replace(/_/g, '/')), (ch) =>
      ch.charCodeAt(0),
    );
    const cose = cbor(
      new CborMap([
        [1, 2],
        [3, -7],
        [-1, 1],
        [-2, x],
        [-3, y],
      ]),
    );
    const rawId = randomBytes(32);
    const c: Cred = {
      rawId,
      rpId: req.rpId,
      userHandle: req.userHandle,
      key: pair.privateKey,
      counter: 0,
      prfSecret: randomBytes(32),
    };
    this.creds.set(b64url(rawId), c);
    const authData = concat(
      await sha256(utf8(req.rpId)),
      new Uint8Array([this.flags(true)]),
      u32(0),
      new Uint8Array(16),
      new Uint8Array([0, rawId.length]),
      rawId,
      cose,
    );
    const attestationObject = cbor(
      new CborMap([
        ['fmt', 'none'],
        ['attStmt', new CborMap([])],
        ['authData', authData],
      ]),
    );
    return {
      rawId,
      attestationObject,
      clientDataJSON: this.clientData('webauthn.create', req.challenge, req.origin),
      prfEnabled: this.opts.prf !== false,
    };
  }

  async get(req: GetRequest): Promise<GetResult> {
    const c = req.allow.length
      ? req.allow.map((id) => this.creds.get(b64url(id))).find((x) => x?.rpId === req.rpId)
      : [...this.creds.values()].find((x) => x.rpId === req.rpId);
    if (!c) throw new Error('NotAllowedError: no credential');
    c.counter++;
    const authData = concat(
      await sha256(utf8(req.rpId)),
      new Uint8Array([this.flags(false)]),
      u32(c.counter),
    );
    const clientDataJSON = this.clientData('webauthn.get', req.challenge, req.origin);
    const raw = new Uint8Array(
      await subtle.sign(
        { name: 'ECDSA', hash: 'SHA-256' },
        c.key,
        concat(authData, await sha256(clientDataJSON)) as BufferSource,
      ),
    );
    const salt = req.prfByCredential?.get(b64url(c.rawId)) ?? req.prfSalt;
    const prf = await this.prf(c, salt);
    return {
      rawId: c.rawId,
      authenticatorData: authData,
      clientDataJSON,
      signature: der(raw),
      userHandle: c.userHandle,
      ...(prf ? { prf } : {}),
    };
  }
}
