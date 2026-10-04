// Shared test set-up: a fresh server claimed by an admin with full rights on fs 1 and 2.
import { type Client, claim, connect, fullGrants, member, type Session } from '../src/index.js';
import {
  type ServerOptions,
  spawnServer,
  type TestServer,
  type TestUser,
  testUser,
} from '../src/testing/index.js';
import { initWasm } from '../src/wasm.js';

export interface World {
  server: TestServer;
  client: Client;
  admin: TestUser;
  session: Session;
}

/** Start a server, claim it as user seed 1 (origin `http://localhost:port`), sign in by device. */
export async function world(opts: ServerOptions = {}): Promise<World> {
  await initWasm();
  const server = await spawnServer(opts);
  const client = await connect(server.url, { origin: `http://localhost:${server.port}` });
  const admin = testUser(1);
  await claim(client, server.claimToken, admin.identity, {
    members: [member(admin.identity, [admin.cert])],
    grants: [...fullGrants(1, admin.fp), ...fullGrants(2, admin.fp)],
  });
  const session = await client.signInDevice(admin);
  return { server, client, admin, session };
}
