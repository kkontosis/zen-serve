// @zen/client/testing: utilities for tests (not for production use).
export { SoftAuthenticator, type SoftAuthenticatorOptions } from './authenticator.js';
export { type CborItem, CborMap, cbor } from './cbor.js';
export { type TestCerts, type TestKeyKind, testCerts } from './certs.js';
export { type TestUser, testUser } from './fixtures.js';
export {
  freePort,
  type ServerOptions,
  type ServiceSpec,
  serverBinary,
  serviceToml,
  spawnServer,
  startIn,
  type TestServer,
} from './server.js';
export { runSsh, type SshRun, type TestSshKeys, testSshKeys } from './ssh.js';
