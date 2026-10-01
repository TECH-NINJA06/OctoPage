// OctoPage in the browser, the worker side: SQLite's official WebAssembly build reading a
// database straight out of a GitHub repository, page by page, read-only.
//
// Pages come from GitHub's raw CDN by path at a commit
// (`https://raw.githubusercontent.com/OWNER/NAME/<commit>/pages/AA/BB/CC`), which serves any
// public repository to browsers and does not count against the REST API's rate limit. SQLite
// asks for pages synchronously, which a worker may do: a synchronous request, on a miss.
// Pages at a commit never change, so they are cached for good: in memory, and in IndexedDB
// across visits. The head comes from the REST API (one call per open), and a database that
// moved to a new generation is followed.
//
// The file is as long as the highest page the page store's free map marks in use, and page 1
// is shown with SQLite's size and change-counter fields filled in, as the Rust VFS does: the
// stored page 1 keeps them only as of the last commit that changed something else in it.
//
// Plain databases only: a compressed or encrypted one is refused.

const HEADER = 32; // OctoPage's page header
const MAGIC = [0x4f, 0x50, 0x47, 0x31]; // "OPG1"
const PACKED = [0x4f, 0x50, 0x58, 0x31]; // "OPX1": compressed or encrypted
const SQLITE_IOERR_SHORT_READ = 522;
// The change counter shown in page 1. The file never changes (it is opened immutable), so
// any value does, as long as the version-valid-for field matches it.
const COUNTER = 2;

let sqlite3 = null;
let state = null;
let lastError = null;

const hex2 = (b) => b.toString(16).padStart(2, '0');
const startsWith = (bytes, prefix) => prefix.every((b, i) => bytes[i] === b);

/** Pages kept across visits, by repository, commit and path. */
class Cache {
  static open() {
    return new Promise((resolve) => {
      if (!self.indexedDB) return resolve(null);
      const request = indexedDB.open('octopage', 1);
      request.onupgradeneeded = () => request.result.createObjectStore('pages');
      request.onsuccess = () => resolve(new Cache(request.result));
      request.onerror = () => resolve(null); // private browsing, say: no cache
    });
  }

  constructor(db) {
    this.db = db;
    this.queue = [];
    this.stored = Promise.resolve();
  }

  /** Everything cached under `prefix`. */
  load(prefix) {
    return new Promise((resolve) => {
      const found = new Map();
      const range = IDBKeyRange.bound(prefix, `${prefix}￿`);
      const cursor = this.db.transaction('pages').objectStore('pages').openCursor(range);
      cursor.onsuccess = () => {
        const c = cursor.result;
        if (!c) return resolve(found);
        found.set(c.key.slice(prefix.length), c.value);
        c.continue();
      };
      cursor.onerror = () => resolve(found);
    });
  }

  /**
   * Keep a page. Pages arrive during a query, while the worker is busy; they are written
   * together, in one transaction, once it is free again.
   */
  put(key, value) {
    if (this.queue.length === 0) setTimeout(() => this.flush(), 0);
    this.queue.push([key, value]);
  }

  /** Write what is waiting. Resolves once everything handed to `put` is stored (or failed). */
  flush() {
    const entries = this.queue.splice(0);
    if (entries.length > 0) {
      const written = new Promise((resolve) => {
        try {
          const tx = this.db.transaction('pages', 'readwrite');
          const pages = tx.objectStore('pages');
          for (const [key, value] of entries) pages.put(value, key);
          tx.oncomplete = tx.onerror = tx.onabort = () => resolve();
        } catch {
          resolve(); // a full or closed store only means no cache
        }
      });
      this.stored = this.stored.then(() => written);
    }
    return this.stored;
  }
}

/** A database's pages at one commit. */
class Pages {
  constructor({ raw, repo, commit, cache, cached }) {
    this.raw = raw.replace(/\/$/, '');
    this.repo = repo;
    this.commit = commit;
    this.cache = cache;
    this.memory = cached ?? new Map();
    this.fetched = 0;
    this.hits = 0;
  }

  get prefix() {
    return `${this.repo}@${this.commit}/`;
  }

  /** A file of the commit's tree, or null if it has none: synchronously. */
  file(path) {
    if (this.memory.has(path)) {
      this.hits++;
      return this.memory.get(path);
    }
    const request = new XMLHttpRequest();
    request.open('GET', `${this.raw}/${this.repo}/${this.commit}/${path}`, false);
    request.responseType = 'arraybuffer';
    request.send();
    let bytes = null;
    if (request.status === 200) bytes = new Uint8Array(request.response);
    else if (request.status !== 404) throw new Error(`${path}: HTTP ${request.status}`);
    this.fetched++;
    this.memory.set(path, bytes);
    this.cache?.put(this.prefix + path, bytes);
    return bytes;
  }

  /** Page `id` as stored (header included), or null. */
  page(id) {
    const path = `pages/${hex2((id >> 16) & 255)}/${hex2((id >> 8) & 255)}/${hex2(id & 255)}`;
    const bytes = this.file(path);
    if (!bytes) return null;
    if (startsWith(bytes, PACKED)) {
      throw new Error('this database is compressed or encrypted: the browser reads plain ones');
    }
    if (!startsWith(bytes, MAGIC)) throw new Error(`page ${id} is not an OctoPage page`);
    const stored = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(8, true);
    if (stored !== id) throw new Error(`page ${id} says it is page ${stored}`);
    return bytes;
  }
}

/**
 * The highest page in use: the last bit set in the free map below the superblock's high-water
 * mark (`next_free`). Free-map page `1 + id / bits` holds page `id`'s bit, least significant
 * first; a free-map page that was never written has no bits set.
 */
function lastPage(pages, { pageSize, mapPages, nextFree }) {
  const bits = (pageSize - HEADER) * 8;
  let id = nextFree - 1;
  while (id > mapPages) {
    const first = id - (id % bits);
    const map = pages.page(1 + first / bits)?.subarray(HEADER);
    for (; map && id >= first && id > mapPages; id--) {
      const bit = id - first;
      if (map[bit >> 3] & (1 << (bit & 7))) return id;
    }
    id = first - 1;
  }
  return mapPages;
}

/** Page 1 as SQLite expects it: the database's size and a change counter in its header. */
function present(stored, pageSize, size) {
  const image = new Uint8Array(pageSize);
  image.set(stored.subarray(HEADER, pageSize));
  const header = new DataView(image.buffer);
  header.setUint32(24, COUNTER); // big-endian, as SQLite's header is
  header.setUint32(28, size);
  header.setUint32(92, COUNTER);
  return image;
}

/**
 * SQLite's page `number`: the stored payload, then OctoPage's 32 reserved bytes (zero). Null
 * past the end of the file, and for a page SQLite never wrote (it reads as zeros).
 */
function sqlitePage(number) {
  if (number < 1 || number > state.size) return null;
  if (number === 1) return state.page1;
  const stored = state.pages.page(state.mapPages + number);
  if (!stored) return null;
  const image = new Uint8Array(state.pageSize);
  image.set(stored.subarray(HEADER, state.pageSize));
  return image;
}

async function head(api, repo, branch) {
  const response = await fetch(`${api.replace(/\/$/, '')}/repos/${repo}/git/ref/heads/${branch}`, {
    headers: { Accept: 'application/vnd.github+json' },
  });
  if (!response.ok) throw new Error(`${repo}: no branch ${branch} (HTTP ${response.status})`);
  return (await response.json()).object.sha;
}

async function where(options) {
  let { repo } = options;
  const raw = options.raw.replace(/\/$/, '');
  let commit = options.commit ?? (await head(options.api, repo, options.branch));
  // A database that moved to a new generation leaves a `moved` file at its last commit.
  for (let hops = 0; hops < 16; hops++) {
    const response = await fetch(`${raw}/${repo}/${commit}/moved`);
    if (response.status !== 200) return { repo, commit };
    repo = (await response.json()).location;
    commit = await head(options.api, repo, options.branch);
  }
  throw new Error('the generation pointers go round in a loop');
}

function installVfs() {
  const { capi, wasm } = sqlite3;
  const fail = (error, code = capi.SQLITE_IOERR) => {
    lastError = error;
    return code;
  };
  const io = new capi.sqlite3_io_methods();
  io.$iVersion = 1;
  sqlite3.vfs.installVfs({
    io: {
      struct: io,
      methods: {
        xClose: () => 0,
        xRead(pFile, pDest, n, offset64) {
          try {
            const { pageSize } = state;
            const offset = Number(offset64);
            let short = false;
            for (let done = 0; done < n; ) {
              const pos = offset + done;
              const within = pos % pageSize;
              const take = Math.min(pageSize - within, n - done);
              const image = sqlitePage(Math.floor(pos / pageSize) + 1);
              const heap = wasm.heap8u(); // after the fetch: memory may have grown
              const dest = Number(pDest) + done;
              if (image) {
                heap.set(image.subarray(within, within + take), dest);
              } else {
                heap.fill(0, dest, dest + take);
                short = true;
              }
              done += take;
            }
            return short ? SQLITE_IOERR_SHORT_READ : 0;
          } catch (error) {
            return fail(error);
          }
        },
        xWrite: () => capi.SQLITE_READONLY,
        xTruncate: () => capi.SQLITE_READONLY,
        xSync: () => 0,
        xFileSize(pFile, pSize) {
          wasm.poke64(pSize, BigInt(state.size * state.pageSize));
          return 0;
        },
        xLock: () => 0,
        xUnlock: () => 0,
        xCheckReservedLock(pFile, pOut) {
          wasm.poke32(pOut, 0);
          return 0;
        },
        xFileControl: () => capi.SQLITE_NOTFOUND,
        xSectorSize: () => 4096,
        xDeviceCharacteristics: () => capi.SQLITE_IOCAP_IMMUTABLE,
      },
    },
  });
  const vfs = new capi.sqlite3_vfs();
  const fallback = new capi.sqlite3_vfs(capi.sqlite3_vfs_find(null));
  vfs.$iVersion = 2;
  vfs.$szOsFile = capi.sqlite3_file.structInfo.sizeof;
  vfs.$mxPathname = 512;
  vfs.$zName = wasm.allocCString('octopage');
  vfs.$xRandomness = fallback.$xRandomness;
  vfs.$xSleep = fallback.$xSleep;
  fallback.dispose();
  sqlite3.vfs.installVfs({
    vfs: {
      struct: vfs,
      methods: {
        xOpen(pVfs, zName, pFile, flags, pOutFlags) {
          const file = new capi.sqlite3_file(pFile);
          file.$pMethods = io.pointer;
          file.dispose();
          wasm.poke32(pOutFlags, capi.SQLITE_OPEN_READONLY);
          return 0;
        },
        xDelete: () => 0,
        xAccess(pVfs, zName, flags, pOut) {
          wasm.poke32(pOut, 0);
          return 0;
        },
        xFullPathname: (pVfs, zName, nOut, pOut) =>
          wasm.cstrncpy(pOut, zName, nOut) < nOut ? 0 : capi.SQLITE_CANTOPEN,
        xCurrentTime(pVfs, pOut) {
          wasm.poke(pOut, 2440587.5 + Date.now() / 864e5, 'double');
          return 0;
        },
        xCurrentTimeInt64(pVfs, pOut) {
          wasm.poke(pOut, 0xbfc83e532200 + Date.now(), 'i64');
          return 0;
        },
        xGetLastError: () => 0,
      },
    },
  });
}

async function open(options) {
  if (!sqlite3) {
    const { default: init } = await import(options.sqlite);
    sqlite3 = await init({ print: () => {}, printErr: () => {} });
    installVfs();
  }
  const { repo, commit } = await where(options);
  const cache = options.cache === false ? null : await Cache.open();
  const cached = cache ? await cache.load(`${repo}@${commit}/`) : null;
  const pages = new Pages({ raw: options.raw, repo, commit, cache, cached });
  const superblock = pages.file('pages/00/00/00');
  if (!superblock) throw new Error(`${repo} has no OctoPage database at ${commit}`);
  const view = new DataView(superblock.buffer, superblock.byteOffset, superblock.byteLength);
  const layout = {
    pageSize: view.getUint32(HEADER + 4, true),
    nextFree: view.getUint32(HEADER + 8, true),
    mapPages: view.getUint32(HEADER + 12, true),
  };
  if (view.getUint32(HEADER + 16, true) !== 0) {
    throw new Error('this database is compressed or encrypted: the browser reads plain ones');
  }
  // SQLite page n is page-store page map_pages + n.
  const size = lastPage(pages, layout) - layout.mapPages;
  let page1 = null;
  if (size > 0) {
    const stored = pages.page(layout.mapPages + 1);
    if (!stored) throw new Error(`${repo} at ${commit}: the database has no page 1`);
    page1 = present(stored, layout.pageSize, size);
  }
  state?.db?.close();
  state = { pages, ...layout, size, page1, repo, commit, db: null };
  state.db = new sqlite3.oo1.DB({
    filename: 'file:octopage.db?immutable=1',
    flags: 'r',
    vfs: 'octopage',
  });
  return { repo, commit, pageSize: layout.pageSize, pages: size };
}

/** Close the database, once the pages it fetched are in the cache. */
async function close() {
  await state?.pages.cache?.flush();
  state?.db?.close();
  state = null;
  return null;
}

function query({ sql, params }) {
  const columns = [];
  lastError = null;
  try {
    const rows = state.db.exec({
      sql,
      bind: params ?? [],
      rowMode: 'array',
      returnValue: 'resultRows',
      columnNames: columns,
    });
    return { columns, rows };
  } catch (error) {
    throw lastError ?? error;
  }
}

self.onmessage = async ({ data: { id, type, options } }) => {
  try {
    let result;
    if (type === 'open') result = await open(options);
    else if (type === 'query') result = query(options);
    else if (type === 'stats') result = { fetched: state.pages.fetched, hits: state.pages.hits };
    else if (type === 'close') result = await close();
    else throw new Error(`unknown request ${type}`);
    self.postMessage({ id, result });
  } catch (error) {
    self.postMessage({ id, error: String(error?.message ?? error) });
  }
};
