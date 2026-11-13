// @zen/sw: serving zen-fs over HTTP from a service worker (spec/sw.md).
// The core: a request in, a response out, over the mounts' replicas.
// Worker-safe: no DOM or Node imports (tsconfig.worker.json checks it).
export { etagOf, handle, type Site } from './handle.js';
export { SANDBOX_CSP, SANDBOX_HEADERS, sandboxHeaders } from './headers.js';
export { contentType, validMediaType } from './mime.js';
export { type Mount, parseMounts, type Route, route } from './mounts.js';
export { escapeHtml } from './pages.js';
export { parseRange, type Range } from './range.js';
