// Connecting and signing in (spec/api.md §2–§3, spec/auth.md).
import type { DeviceSecret, Info, SigningIdentity, Session as WireSession } from '@zen/wasm';
import { b64url, utf8 } from './bytes.js';
import { ZenError } from './errors.js';
import { type Authenticator, passkeyGet } from './passkey.js';
import { Session, type UnlockMaterial } from './session.js';
import { Transport, type TransportOptions } from './transport.js';
import { initWasm, zw } from './wasm.js';

/** Options of `connect`. */
export interface ConnectOptions extends TransportOptions {
  /**
   * The server origin as this client sees it, `scheme://host[:port]`
   * (auth.md §5). Sign-ins are bound to it. Default: `location.origin` in a
   * browser page served by the server, else the URL's origin.
   */
  origin?: string;
  /** Where to load the WASM module from (browser); see `initWasm`. */
  wasm?: URL | string | Response | BufferSource;
}

/** Connect: load the WASM core and return a client for `url`. Makes no request. */
export async function connect(url: string, opts: ConnectOptions = {}): Promise<Client> {
  await initWasm(opts.wasm);
  const origin = (opts.origin ?? new URL(url).origin).toLowerCase();
  if (!zw.validOrigin(origin)) throw new ZenError(0, 'bad_origin', origin);
  return new Client(new Transport(url, opts), origin);
}

/** The material for a device sign-in (method 1). */
export interface DeviceCredentials {
  /** The user's identity (only its public half is sent). */
  identity: SigningIdentity | Uint8Array;
  /** The device's secret. */
  device: DeviceSecret;
  /** The device certificate the user issued (formats.md §7.4), listed in the ACL. */
  cert: Uint8Array;
}

/** An unauthenticated client of one server. */
export class Client {
  constructor(
    readonly transport: Transport,
    /** The origin sign-ins are bound to. */
    readonly origin: string,
  ) {}

  /** `GET /v1/info`. */
  async info(): Promise<Info> {
    return zw.decodeInfo(await this.transport.get('/v1/info'));
  }

  /** A fresh single-use sign-in challenge (§3.1). */
  async challenge(): Promise<Uint8Array> {
    const c = await this.transport.call(
      '/v1/auth/challenge',
      zw.encodeEmpty,
      zw.decodeChallenge,
      {},
    );
    return c.challenge;
  }

  private session(s: WireSession, unlock: UnlockMaterial = {}): Session {
    return new Session(this, b64url(s.token), s, unlock);
  }

  /** Sign in with a device key (method 1, auth.md §6). */
  async signInDevice(c: DeviceCredentials): Promise<Session> {
    const challenge = await this.challenge();
    const user = c.identity instanceof Uint8Array ? c.identity : c.identity.publicIdentity;
    const s = await this.transport.call(
      '/v1/auth/session',
      zw.encodeSessionRequest,
      zw.decodeSession,
      {
        challenge,
        origin: this.origin,
        user,
        cert: c.cert,
        sig: c.device.signSession(challenge, this.origin),
      },
    );
    return this.session(s);
  }

  /** The salt and Argon2id parameters of a login name (method 6, §3.5). */
  async passwordParams(name: string) {
    return this.transport.call(
      '/v1/auth/password/params',
      zw.encodePasswordParamsRequest,
      zw.decodePasswordParams,
      { name },
    );
  }

  /** Sign in with a password-derived key (method 6, the default; auth.md §11). */
  async signInPassword(name: string, password: string): Promise<Session> {
    const p = await this.passwordParams(name);
    const key = zw.PasswordKey.derive(utf8(password), p.salt, p.m_cost_kib, p.t_cost, p.p_cost);
    try {
      const challenge = await this.challenge();
      const s = await this.transport.call(
        '/v1/auth/password/session',
        zw.encodePasswordSessionRequest,
        zw.decodeSession,
        { name, challenge, origin: this.origin, sig: key.signSession(challenge, this.origin) },
      );
      return this.session(s);
    } finally {
      key.free();
    }
  }

  /**
   * Sign in with OPAQUE (method 3, auth.md §8). The session keeps the
   * export key, which opens the credential's type-5 keyslot.
   */
  async signInOpaque(name: string, password: string): Promise<Session> {
    const pw = utf8(password);
    const login = zw.OpaqueLogin.start(pw);
    try {
      const r = await this.transport.call(
        '/v1/auth/opaque/login/start',
        zw.encodeOpaqueLoginStart,
        zw.decodeOpaqueLoginResponse,
        { name, origin: this.origin, request: login.request },
      );
      let f: ReturnType<typeof login.finish>;
      try {
        f = login.finish(pw, r.response, r.m_cost_kib, r.t_cost, r.p_cost, this.origin);
      } catch {
        // The server's response doesn't open with this password: the same
        // answer as the other methods give for any credential failure.
        throw new ZenError(401, 'unauthorized', 'unknown login name or wrong password');
      }
      const s = await this.transport.call(
        '/v1/auth/opaque/login/finish',
        zw.encodeOpaqueLoginFinish,
        zw.decodeSession,
        { state: r.state, finalization: f.message },
      );
      return this.session(s, { opaqueExportKey: f.exportKey });
    } finally {
      login.free();
    }
  }

  /**
   * Sign in with a passkey (method 2, auth.md §7). With `user`, the
   * authenticator is asked only for that user's passkeys, and `prfSalts`
   * (type-4 slot salts by WebAuthn credential id, base64url) let one touch
   * also return the PRF output that opens a keyslot (auth.md §7.7).
   */
  async signInPasskey(
    authenticator: Authenticator,
    opts: { user?: Uint8Array; prfSalts?: Map<string, Uint8Array> } = {},
  ): Promise<Session> {
    const begin = await this.transport.call(
      '/v1/auth/passkey/session/begin',
      zw.encodePasskeyBegin,
      zw.decodePasskeyRequest,
      opts.user ? { user: opts.user } : {},
    );
    const a = await passkeyGet(authenticator, this.origin, begin, opts.prfSalts);
    const s = await this.transport.call(
      '/v1/auth/passkey/session',
      zw.encodePasskeySession,
      zw.decodeSession,
      {
        credential_id: a.rawId,
        authenticator_data: a.authenticatorData,
        client_data_json: a.clientDataJSON,
        signature: a.signature,
        ...(a.userHandle ? { user_handle: a.userHandle } : {}),
      },
    );
    const unlock: UnlockMaterial = {};
    if (a.prf) unlock.prf = { credentialId: zw.passkeyCredentialId(a.rawId), output: a.prf };
    return this.session(s, unlock);
  }

  /**
   * Sign in with the TLS client certificate of this connection (method 5,
   * auth.md §10). In Node, pass an undici dispatcher with the certificate in
   * `fetchInit`; a browser presents its own.
   */
  async signInMtls(): Promise<Session> {
    const s = await this.transport.call(
      '/v1/auth/mtls/session',
      zw.encodeEmpty,
      zw.decodeSession,
      {},
    );
    return this.session(s);
  }

  /** Use an API token (method 4, auth.md §9): no sign-in, the token is the bearer. */
  withApiToken(token: string): Session {
    return new Session(this, token, undefined, {});
  }

  /** Resume a session from its token (from `session.token`). */
  resume(token: string): Session {
    return new Session(this, token, undefined, {});
  }
}
