// OctoPage in the browser: query a database kept in a public GitHub repository, read-only,
// without a server. SQLite (its official WebAssembly build) runs in a Web Worker and reads
// pages from GitHub's raw CDN as queries need them.
//
//   import { openDatabase } from './octopage.mjs';
//   const db = await openDatabase({ repo: 'OWNER/NAME' });
//   const { columns, rows } = await db.query('SELECT * FROM notes WHERE id = ?', [1]);
//
// `commit` opens the database as of that commit (time travel); otherwise the head of
// `branch` (default "main"), read with one REST call (unauthenticated: 60 an hour).

const SQLITE = 'https://cdn.jsdelivr.net/npm/@sqlite.org/sqlite-wasm@3.53.4-build1/dist/index.mjs';

/**
 * Open a database. Options: `repo` ("OWNER/NAME", required), `branch`, `commit`, `raw` (the
 * raw CDN, default https://raw.githubusercontent.com), `api` (default https://api.github.com),
 * `sqlite` (the URL of SQLite's WebAssembly module), `cache` (false: no IndexedDB cache).
 */
export async function openDatabase(options) {
  if (!options?.repo) throw new Error('openDatabase needs a repo, "OWNER/NAME"');
  const worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
  const pending = new Map();
  let next = 0;
  worker.onmessage = ({ data }) => {
    const call = pending.get(data.id);
    pending.delete(data.id);
    if (data.error !== undefined) call.reject(new Error(data.error));
    else call.resolve(data.result);
  };
  worker.onerror = (event) => {
    for (const call of pending.values()) call.reject(new Error(event.message));
    pending.clear();
  };
  const call = (type, payload) =>
    new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { resolve, reject });
      worker.postMessage({ id, type, options: payload });
    });
  const opened = await call('open', {
    repo: options.repo,
    branch: options.branch ?? 'main',
    commit: options.commit,
    raw: options.raw ?? 'https://raw.githubusercontent.com',
    api: options.api ?? 'https://api.github.com',
    sqlite: options.sqlite ?? SQLITE,
    cache: options.cache,
  });
  return {
    /** Where the database was read: its repository (after following moves) and commit. */
    repo: opened.repo,
    commit: opened.commit,
    pageSize: opened.pageSize,
    pages: opened.pages,
    /** Run a query: `{columns, rows}`. */
    query: (sql, params) => call('query', { sql, params }),
    /** Pages fetched from the CDN, and read from the cache. */
    stats: () => call('stats'),
    /** Close the database, once the pages it fetched are cached for the next visit. */
    async close() {
      try {
        await call('close');
      } finally {
        worker.terminate();
      }
    },
  };
}
