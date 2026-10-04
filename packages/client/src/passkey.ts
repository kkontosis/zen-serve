// Passkeys (sign-in method 2, auth.md §7) through a pluggable authenticator.
import type { PasskeyCreation, PasskeyRequest } from '@zen/wasm';
import { b64url, fromUtf8 } from './bytes.js';

/** What `navigator.credentials.create` needs (and returns), in bytes. */
export interface CreateRequest {
  origin: string;
  rpId: string;
  challenge: Uint8Array;
  userHandle: Uint8Array;
  userName: string;
  /** COSE algorithms, preferred first. */
  algorithms: number[];
  exclude: Uint8Array[];
  userVerification: string;
  /** Ask for the PRF extension's output for this salt at creation (if supported). */
  prfSalt?: Uint8Array;
}

export interface CreateResult {
  rawId: Uint8Array;
  attestationObject: Uint8Array;
  clientDataJSON: Uint8Array;
  /** The PRF output for `prfSalt`, when the authenticator gave one. */
  prf?: Uint8Array;
  /** Whether the authenticator supports PRF. */
  prfEnabled?: boolean;
}

/** What `navigator.credentials.get` needs (and returns), in bytes. */
export interface GetRequest {
  origin: string;
  rpId: string;
  challenge: Uint8Array;
  /** allowCredentials; empty for a discoverable sign-in. */
  allow: Uint8Array[];
  userVerification: string;
  /** PRF salts by WebAuthn credential id (base64url): `evalByCredential`. */
  prfByCredential?: Map<string, Uint8Array>;
  /** One PRF salt for any credential: `eval`. */
  prfSalt?: Uint8Array;
}

export interface GetResult {
  rawId: Uint8Array;
  authenticatorData: Uint8Array;
  clientDataJSON: Uint8Array;
  signature: Uint8Array;
  userHandle?: Uint8Array;
  /** The PRF output (`results.first`), when one was asked for and given. */
  prf?: Uint8Array;
}

/** A WebAuthn authenticator: the browser's, or a software one for Node and tests. */
export interface Authenticator {
  create(req: CreateRequest): Promise<CreateResult>;
  get(req: GetRequest): Promise<GetResult>;
}

/** Run a sign-in ceremony for a `passkey/session/begin` response. */
export async function passkeyGet(
  a: Authenticator,
  origin: string,
  begin: PasskeyRequest,
  prfSalts?: Map<string, Uint8Array>,
): Promise<GetResult> {
  return a.get({
    origin,
    rpId: begin.rp_id,
    challenge: begin.challenge,
    allow: begin.allow,
    userVerification: begin.user_verification,
    ...(prfSalts?.size ? { prfByCredential: prfSalts } : {}),
  });
}

/** Run a registration ceremony for a `passkey/register/begin` response. */
export async function passkeyCreate(
  a: Authenticator,
  origin: string,
  begin: PasskeyCreation,
  userName: string,
  prfSalt?: Uint8Array,
): Promise<CreateResult> {
  return a.create({
    origin,
    rpId: begin.rp_id,
    challenge: begin.challenge,
    userHandle: begin.user_handle,
    userName,
    algorithms: begin.algorithms.map(Number),
    exclude: begin.exclude,
    userVerification: begin.user_verification,
    ...(prfSalt ? { prfSalt } : {}),
  });
}

const buf = (b: Uint8Array): ArrayBuffer => b.slice().buffer as ArrayBuffer;
const bytes = (b: ArrayBuffer | ArrayBufferView): Uint8Array =>
  b instanceof ArrayBuffer
    ? new Uint8Array(b)
    : new Uint8Array(b.buffer, b.byteOffset, b.byteLength);

/** The browser's authenticator, through `navigator.credentials`. */
export function browserAuthenticator(): Authenticator {
  return {
    async create(req) {
      const cred = (await navigator.credentials.create({
        publicKey: {
          rp: { id: req.rpId, name: req.rpId },
          user: { id: buf(req.userHandle), name: req.userName, displayName: req.userName },
          challenge: buf(req.challenge),
          pubKeyCredParams: req.algorithms.map((alg) => ({ type: 'public-key', alg })),
          excludeCredentials: req.exclude.map((id) => ({ type: 'public-key', id: buf(id) })),
          authenticatorSelection: {
            residentKey: 'preferred',
            userVerification: req.userVerification as UserVerificationRequirement,
          },
          extensions: req.prfSalt
            ? ({
                prf: { eval: { first: buf(req.prfSalt) } },
              } as AuthenticationExtensionsClientInputs)
            : ({ prf: {} } as AuthenticationExtensionsClientInputs),
        },
      })) as PublicKeyCredential | null;
      if (!cred) throw new Error('passkey creation cancelled');
      const r = cred.response as AuthenticatorAttestationResponse;
      const ext = cred.getClientExtensionResults() as {
        prf?: { enabled?: boolean; results?: { first?: ArrayBuffer } };
      };
      return {
        rawId: bytes(cred.rawId),
        attestationObject: bytes(r.attestationObject),
        clientDataJSON: bytes(r.clientDataJSON),
        ...(ext.prf?.results?.first ? { prf: bytes(ext.prf.results.first) } : {}),
        ...(ext.prf?.enabled !== undefined ? { prfEnabled: ext.prf.enabled } : {}),
      };
    },
    async get(req) {
      let prf: Record<string, unknown> | undefined;
      if (req.prfByCredential?.size) {
        const by: Record<string, { first: ArrayBuffer }> = {};
        for (const [id, salt] of req.prfByCredential) by[id] = { first: buf(salt) };
        prf = { evalByCredential: by };
      } else if (req.prfSalt) {
        prf = { eval: { first: buf(req.prfSalt) } };
      }
      const cred = (await navigator.credentials.get({
        publicKey: {
          rpId: req.rpId,
          challenge: buf(req.challenge),
          allowCredentials: req.allow.map((id) => ({ type: 'public-key', id: buf(id) })),
          userVerification: req.userVerification as UserVerificationRequirement,
          ...(prf ? { extensions: { prf } as AuthenticationExtensionsClientInputs } : {}),
        },
      })) as PublicKeyCredential | null;
      if (!cred) throw new Error('passkey sign-in cancelled');
      const r = cred.response as AuthenticatorAssertionResponse;
      const ext = cred.getClientExtensionResults() as {
        prf?: { results?: { first?: ArrayBuffer } };
      };
      return {
        rawId: bytes(cred.rawId),
        authenticatorData: bytes(r.authenticatorData),
        clientDataJSON: bytes(r.clientDataJSON),
        signature: bytes(r.signature),
        ...(r.userHandle ? { userHandle: bytes(r.userHandle) } : {}),
        ...(ext.prf?.results?.first ? { prf: bytes(ext.prf.results.first) } : {}),
      };
    },
  };
}

/** The `challenge` of a clientDataJSON (for software authenticators' tests). */
export function clientDataChallenge(clientDataJSON: Uint8Array): string {
  return (JSON.parse(fromUtf8(clientDataJSON)) as { challenge: string }).challenge;
}

export { b64url as credentialKey };
