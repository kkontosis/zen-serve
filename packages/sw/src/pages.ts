// The pages the worker writes itself (spec/sw.md §3, §7): errors and
// listings. Static HTML, no scripts; every name is escaped.

/** HTML-escape text for an element or a double-quoted attribute. */
export function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!,
  );
}

function page(title: string, body: string): string {
  return (
    '<!doctype html>\n<html><head><meta charset="utf-8">' +
    '<meta name="viewport" content="width=device-width">' +
    `<title>${escapeHtml(title)}</title></head>\n<body>\n${body}\n</body></html>\n`
  );
}

/** The errors of sw.md §7 (and 416). */
export type ErrorKind =
  | 400
  | 404
  | 405
  | 416
  | 500
  | 'integrity'
  | 'locked'
  | 'offline'
  | 'starting';

const ERRORS: Record<
  string,
  { status: number; title: string; text: string; reason?: string; more?: string }
> = {
  400: { status: 400, title: 'Bad request', text: 'This address is not a valid path.' },
  404: { status: 404, title: 'Not found', text: 'Nothing is here.' },
  405: { status: 405, title: 'Method not allowed', text: 'Only GET and HEAD are served.' },
  416: { status: 416, title: 'Range not satisfiable', text: 'The range is past the end.' },
  500: { status: 500, title: 'Error', text: 'The file could not be served.' },
  integrity: {
    status: 502,
    title: 'Integrity error',
    text: 'The file failed to verify, so it is not shown.',
    reason: 'integrity',
  },
  locked: {
    status: 503,
    title: 'Locked',
    text: 'This browser is not set up to view these files yet.',
    reason: 'locked',
    more: '<p><a href="/_zen/enroll.html">Set it up</a></p>',
  },
  offline: {
    status: 503,
    title: 'Offline',
    text: 'This file is not kept here, and the server cannot be reached.',
    reason: 'offline',
  },
  starting: {
    status: 503,
    title: 'Starting',
    text: 'The viewer is starting. Try again in a moment.',
    reason: 'starting',
  },
};

/** An error: its status, `Zen-Reason` and page. */
export function errorPage(kind: ErrorKind): { status: number; reason?: string; html: string } {
  const e = ERRORS[String(kind)]!;
  const html = page(e.title, `<h1>${e.title}</h1>\n<p>${e.text}</p>${e.more ?? ''}`);
  return { status: e.status, ...(e.reason ? { reason: e.reason } : {}), html };
}

/** A directory's entry in a listing. */
export interface Listed {
  name: string;
  dir: boolean;
}

/**
 * A listing (sw.md §3): directories first, each group sorted by name, names
 * escaped and links percent-encoded; `../` first unless at the mount's base.
 */
export function listingPage(title: string, entries: Listed[], parent: boolean): string {
  const sorted = [...entries].sort((a, b) =>
    a.dir !== b.dir ? (a.dir ? -1 : 1) : a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
  );
  const items = sorted.map((e) => {
    const href = `./${encodeURIComponent(e.name)}${e.dir ? '/' : ''}`;
    return `<li><a href="${escapeHtml(href)}">${escapeHtml(e.name)}${e.dir ? '/' : ''}</a></li>`;
  });
  if (parent) items.unshift('<li><a href="../">../</a></li>');
  return page(title, `<h1>${escapeHtml(title)}</h1>\n<ul>\n${items.join('\n')}\n</ul>`);
}
