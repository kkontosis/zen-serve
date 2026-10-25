// An authenticated session: credentials, the ACL, filesystems, streams.
import type {
  Credential,
  DeviceInfo,
  OriginState,
  SigningIdentity,
  Session as WireSession,
} from '@zen/wasm';
import { Acl } from './acl.js';
import { randomBytes, utf8 } from './bytes.js';
import type { Client, DeviceCredentials } from './client.js';
import { TreeClock } from './clock.js';
import { newDevice } from './devices.js';
import { ZenError } from './errors.js';
import { Fs } from './fs.js';
import type { CredentialVault } from './local/vault.js';
import { type Authenticator, passkeyCreate } from './passkey.js';
import { Stream, type StreamOptions } from './stream.js';
import { type SvcConnectOptions, SvcSocket } from './svc.js';
import type { Dec, Enc } from './transport.js';
import { zw } from './wasm.js';

/** Secrets a sign-in yielded that can open a keyslot without another prompt. */
export interface UnlockMaterial {
  /** OPAQUE export key (opens the type-5 slot of the session's credential). */
  opaqueExportKey?: Uint8Array;
  /** A passkey's PRF output, with its credential-store id (type-4 slot). */
  prf?: { credentialId: Uint8Array; output: Uint8Array };
}

/** Argon2id parameters. */
export interface Argon2 {
  m_cost_kib: number;
  t_cost: number;
  p_cost: number;
}

/** A session: the bearer of every authenticated request. */
export class Session {
  /** The ACL (signed membership log). */
  readonly acl: Acl;

  constructor(
    readonly client: Client,
    /** The `Authorization` bearer: a base64url session token, or an API token. */
    readonly token: string,
    /** The sign-in response; absent for API tokens and resumed sessions. */
    readonly info: WireSession | undefined,
    /** What the sign-in gave for unlocking keyslots. */
    readonly unlock: UnlockMaterial,
  ) {
    this.acl = new Acl(this);
  }

  private sessionClock: TreeClock | undefined;

  /**
   * The session's hybrid logical clock: events, tree operations and CRDT
   * rows all take their timestamps from it. In memory unless `useClock`
   * set a persisted one.
   */
  get clock(): TreeClock {
    this.sessionClock ??= new TreeClock();
    return this.sessionClock;
  }

  /**
   * Use this clock (one with a persisted `ClockStore`) for the session.
   * Call it before anything takes a timestamp.
   */
  useClock(clock: TreeClock): void {
    if (this.sessionClock && this.sessionClock !== clock) {
      throw new Error('the session clock is already in use');
    }
    this.sessionClock = clock;
  }

  private userIdentity: SigningIdentity | undefined;

  /**
   * The user identity (the ACL member identity) for what needs it later:
   * signed header writes (formats.md §12.1) and `approveDevice` without an
   * argument. Kept in memory only; the caller frees it.
   */
  useIdentity(identity: SigningIdentity): void {
    this.userIdentity = identity;
  }

  /** The identity `useIdentity` set, if any. */
  get identity(): SigningIdentity | undefined {
    return this.userIdentity;
  }

  /** The device fingerprint, or the credential id for other methods. */
  get deviceFp(): Uint8Array | undefined {
    return this.info?.device_fp;
  }

  /** The user fingerprint. */
  get userFp(): Uint8Array | undefined {
    return this.info?.user_fp;
  }

  /** An authenticated typed POST. */
  call<Req, Res>(path: string, enc: Enc<Req>, dec: Dec<Res>, req: Req): Promise<Res> {
    return this.client.transport.call(path, enc, dec, req, this.token);
  }

  /** The server's current read version (`/v1/grv`). */
  async readVersion(): Promise<bigint> {
    return (await this.call('/v1/grv', zw.encodeEmpty, zw.decodeReadVersion, {})).read_version;
  }

  /** End the session (§3.4). */
  async logout(): Promise<void> {
    await this.call('/v1/auth/logout', zw.encodeEmpty, zw.decodeEmpty, {});
  }

  /** The configured filesystems the caller has rights on (§4.3). */
  async fsList(): Promise<{ id: number; rights: string[] }[]> {
    return (await this.call('/v1/fs/list', zw.encodeEmpty, zw.decodeFsList, {})).fs;
  }

  /** A filesystem by id. Its keys come from `unlock`. */
  fs(id: number): Fs {
    return new Fs(this, id);
  }

  /** Open the WebSocket stream (§9). */
  stream(opts?: StreamOptions): Promise<Stream> {
    return Stream.open(this, opts);
  }

  /**
   * Connect to a `tcp-auth` service (§14.9): needs topic `append` on its
   * id. Rejects with a `ZenError` (`forbidden`, `not_found`, `quota`, …).
   */
  svcConnect(opts: SvcConnectOptions): Promise<SvcSocket> {
    return SvcSocket.open(this, opts);
  }

  // ---------------------------------------------------------------- devices

  /**
   * Certify a device key with the user identity and register it
   * (api.md §15.2), so its checkpoints verify for every member. Any session
   * of the same user can do it, e.g. a laptop's passkey session approving a
   * phone's device key. `identity` defaults to `useIdentity`'s.
   */
  async approveDevice(
    devicePublic: Uint8Array,
    identity: SigningIdentity | undefined = this.userIdentity,
  ): Promise<{ cert: Uint8Array; fp: Uint8Array }> {
    if (!identity)
      throw new ZenError(0, 'signer_needed', 'approving a device needs the user identity');
    const cert = identity.issueDeviceCert(devicePublic, BigInt(Math.floor(Date.now() / 1000)));
    const r = await this.call(
      '/v1/devices/register',
      zw.encodeDeviceRegister,
      zw.decodeDeviceRegistered,
      { cert },
    );
    return { cert, fp: r.device };
  }

  /**
   * Enroll a new device key (for sessions without one: passkey, OPAQUE, API
   * token, mTLS): make it, certify and register it. With `vault`, it is kept
   * there as the signer (`vault.signer()`). The caller frees `device.device`.
   */
  async enrollDevice(
    identity: SigningIdentity | undefined = this.userIdentity,
    opts: { vault?: CredentialVault } = {},
  ): Promise<{ device: DeviceCredentials; fp: Uint8Array }> {
    const d = newDevice();
    const { cert, fp } = await this.approveDevice(d.devicePublic, identity);
    if (opts.vault) {
      await opts.vault.saveSigner({ identity: identity!, deviceSeed: d.seed, cert });
    }
    d.seed.fill(0);
    return { device: { identity: identity!.publicIdentity, device: d.secret, cert }, fp };
  }

  /** Registered devices (api.md §15.2), as the server reports them (unverified). */
  async devices(fps: Uint8Array[]): Promise<DeviceInfo[]> {
    const r = await this.call('/v1/devices/get', zw.encodeDevicesGet, zw.decodeDeviceList, {
      devices: fps,
    });
    return r.devices;
  }

  // ---------------------------------------------------------------- credentials

  /** The caller's credentials, or a member's (admin) (§3.9). */
  async credentials(user?: Uint8Array): Promise<Credential[]> {
    const r = await this.call(
      '/v1/auth/credentials/list',
      zw.encodeCredentialsList,
      zw.decodeCredentials,
      user ? { user } : {},
    );
    return r.credentials;
  }

  /** Remove a credential; its sessions end (§3.9). */
  async removeCredential(id: Uint8Array): Promise<void> {
    await this.call('/v1/auth/credentials/remove', zw.encodeCredentialId, zw.decodeEmpty, { id });
  }

  /**
   * Register or replace the caller's password-derived key (method 6, §3.7).
   * Parameters default to the server's (`/v1/info`). Returns the credential id.
   */
  async setPassword(name: string, password: string, params?: Argon2): Promise<Uint8Array> {
    const p = params ?? (await this.client.info()).auth?.password_params;
    if (!p) throw new Error('the server advertises no password parameters');
    const key = zw.PasswordKey.create(utf8(password), p.m_cost_kib, p.t_cost, p.p_cost);
    try {
      const r = await this.call(
        '/v1/auth/password/set',
        zw.encodePasswordSet,
        zw.decodeCredentialId,
        {
          name,
          salt: key.salt!,
          m_cost_kib: p.m_cost_kib,
          t_cost: p.t_cost,
          p_cost: p.p_cost,
          identity: key.publicIdentity,
        },
      );
      return r.id;
    } finally {
      key.free();
    }
  }

  /**
   * Register or replace the caller's OPAQUE password (method 3, §3.15).
   * Returns the credential id and the export key, which a type-5 keyslot
   * wraps the fs keys to (re-wrap it after a password change).
   */
  async registerOpaque(
    name: string,
    password: string,
  ): Promise<{ id: Uint8Array; exportKey: Uint8Array }> {
    const pw = utf8(password);
    const reg = zw.OpaqueRegistration.start(pw);
    try {
      const r = await this.call(
        '/v1/auth/opaque/register/start',
        zw.encodeOpaqueRegisterStart,
        zw.decodeOpaqueRegistration,
        { name, request: reg.request },
      );
      const f = reg.finish(pw, r.response, r.m_cost_kib, r.t_cost, r.p_cost);
      const done = await this.call(
        '/v1/auth/opaque/register/finish',
        zw.encodeOpaqueRegisterFinish,
        zw.decodeCredentialId,
        {
          name,
          upload: f.message,
          m_cost_kib: r.m_cost_kib,
          t_cost: r.t_cost,
          p_cost: r.p_cost,
        },
      );
      return { id: done.id, exportKey: f.exportKey };
    } finally {
      reg.free();
    }
  }

  /**
   * Add a passkey (method 2, §3.11). With `prf`, the authenticator is also
   * asked for its PRF output for a fresh salt, for a type-4 keyslot: the
   * result then carries `prf` (absent if the authenticator has no PRF).
   */
  async registerPasskey(
    authenticator: Authenticator,
    opts: { userName?: string; label?: string; prf?: boolean } = {},
  ): Promise<{
    id: Uint8Array;
    rawId: Uint8Array;
    prf?: { salt: Uint8Array; output: Uint8Array };
  }> {
    const begin = await this.call(
      '/v1/auth/passkey/register/begin',
      zw.encodeEmpty,
      zw.decodePasskeyCreation,
      {},
    );
    const salt = opts.prf ? randomBytes(32) : undefined;
    const c = await passkeyCreate(
      authenticator,
      this.client.origin,
      begin,
      opts.userName ?? 'zen',
      salt,
    );
    const r = await this.call(
      '/v1/auth/passkey/register/finish',
      zw.encodePasskeyRegister,
      zw.decodeCredentialId,
      {
        attestation_object: c.attestationObject,
        client_data_json: c.clientDataJSON,
        ...(opts.label !== undefined ? { label: opts.label } : {}),
      },
    );
    let prf = c.prf;
    if (salt && !prf && c.prfEnabled !== false) {
      // Most authenticators give the PRF output only on `get`: ask once more.
      const challenge = await this.client.challenge();
      const g = await authenticator.get({
        origin: this.client.origin,
        rpId: begin.rp_id,
        challenge,
        allow: [c.rawId],
        userVerification: begin.user_verification,
        prfSalt: salt,
      });
      prf = g.prf;
    }
    return {
      id: r.id,
      rawId: c.rawId,
      ...(salt && prf ? { prf: { salt, output: prf } } : {}),
    };
  }

  /** Bind a TLS client certificate to a member (method 5, §3.13). */
  async registerMtls(
    opts: { user?: Uint8Array; cert?: Uint8Array; label?: string } = {},
  ): Promise<Uint8Array> {
    const r = await this.call(
      '/v1/auth/mtls/register',
      zw.encodeMtlsRegister,
      zw.decodeCredentialId,
      opts,
    );
    return r.id;
  }

  /** Create an API token for a member (admins, method 4, §3.8). */
  async createApiToken(opts: {
    user: Uint8Array;
    label?: string;
    expiresUnix?: bigint;
  }): Promise<{ token: string; id: Uint8Array }> {
    const r = await this.call(
      '/v1/auth/tokens/create',
      zw.encodeApiTokenCreate,
      zw.decodeApiToken,
      {
        user: opts.user,
        ...(opts.label !== undefined ? { label: opts.label } : {}),
        ...(opts.expiresUnix !== undefined ? { expires_unix: opts.expiresUnix } : {}),
      },
    );
    return { token: r.token, id: r.id };
  }

  // ---------------------------------------------------------------- admin

  /** The origin policy (admins, §3.10). */
  async origins(): Promise<OriginState> {
    return this.call('/v1/admin/origins/get', zw.encodeEmpty, zw.decodeOriginState, {});
  }

  /** Replace the pinned origins (admins, §3.10). */
  async setPinnedOrigins(pinned: string[]): Promise<void> {
    await this.call('/v1/admin/origins/set', zw.encodeOriginPins, zw.decodeEmpty, { pinned });
  }

  /** Storage health (admins, §11). */
  async status() {
    return this.call('/v1/admin/status', zw.encodeEmpty, zw.decodeClusterStatus, {});
  }
}
