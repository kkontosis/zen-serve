// @zen/client/testing: utilities for tests (not for production use).
export { SoftAuthenticator, type SoftAuthenticatorOptions } from './authenticator.js';
export { type CborItem, CborMap, cbor } from './cbor.js';
export { type TestUser, testUser } from './fixtures.js';
export {
  freePort,
  type ServerOptions,
  serverBinary,
  spawnServer,
  startIn,
  type TestServer,
} from './server.js';
