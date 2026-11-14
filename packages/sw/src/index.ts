// @zen/sw: serving zen-fs over HTTP from a service worker (spec/sw.md).
// The core: a request in, a response out, over the mounts' replicas.
// Worker-safe: no DOM or Node imports (tsconfig.worker.json checks it).

// The worker's start and enrollment (sw.md §8).
export {
  type Enrolled,
  type EnrollMessage,
  type EnrollPageOptions,
  type EnrollViewerOptions,
  enrollPage,
  enrollViewer,
  type MessageLike,
  type Offer,
  type PageResult,
  type PopupLike,
  type PostTarget,
  type WindowLike,
} from './enroll.js';
export { etagOf, handle, type Site } from './handle.js';
export { SANDBOX_CSP, SANDBOX_HEADERS, sandboxHeaders } from './headers.js';
export { contentType, validMediaType } from './mime.js';
export { type Mount, parseMounts, type Route, route } from './mounts.js';
export { escapeHtml } from './pages.js';
export { parseRange, type Range } from './range.js';
export {
  decodeMounts,
  encodeMounts,
  MOUNTS_KEY,
  MOUNTS_STORE,
  PENDING_BYTES,
  START_TIMEOUT_MS,
  type StartOptions,
  startSite,
  type Viewer,
} from './start.js';
