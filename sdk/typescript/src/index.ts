/**
 * A client for the OctoPage service's HTTP API: SQL on databases kept in your GitHub
 * repositories, from any JavaScript runtime with `fetch` (Node 18+, Deno, Bun, browsers,
 * serverless and edge functions). No dependencies.
 *
 * ```ts
 * import { OctoPage } from '@octopage/client';
 *
 * const octopage = new OctoPage({ url: 'https://…', apiKey: process.env.OCTOPAGE_API_KEY! });
 * const db = octopage.database('db_…');
 * await db.execute('INSERT INTO notes(body) VALUES(?)', ['hello']);
 * const notes = await db.query<{ id: number; body: string }>('SELECT * FROM notes');
 * ```
 *
 * Values: SQL integers come back as numbers, or as bigints beyond ±2^53; blobs as
 * `Uint8Array`. A whole JavaScript number is sent as an integer: wrap it in `real()` to send
 * a real.
 */

/** A value as a query returns it. */
export type SqlValue = null | number | bigint | string | Uint8Array;

/** A number to be sent as a SQL real even when it is whole (`real(2)` is `2.0`). */
export class Real {
  constructor(readonly value: number) {}
}

/** `value` as a SQL real. */
export const real = (value: number): Real => new Real(value);

/** A value a statement's parameters may hold. Booleans are 1 and 0; any binary view is a blob. */
export type Param = SqlValue | boolean | Real | ArrayBufferView | undefined;

/** A row as an object, by column name. */
export type Row = Record<string, SqlValue>;

/** What a statement returned. */
export interface Result {
  columns: string[];
  rows: SqlValue[][];
  /** Rows the statement inserted, updated or deleted. */
  changed: number;
  /** The commit a write made (a git commit id), or null. */
  commit: string | null;
}

/** A statement with its parameters, for `batch`. */
export interface Statement {
  sql: string;
  params?: readonly Param[];
}

export interface BatchResult {
  results: Result[];
  commit: string | null;
}

export interface DatabaseInfo {
  id: string;
  repository: string;
  branch: string;
  encryption: 'none' | 'kms' | 'passphrase';
  /** When it was added to the service (seconds since the Unix epoch). */
  created: number;
  /** In the answer to `createDatabase`: whether it was created (not just registered). */
  new?: boolean;
  head?: string;
  page_size?: number;
  retention?: string;
  /** Once only, when an encrypted database is created: keep it safe. */
  recovery_key?: string;
}

export interface NewDatabase {
  /** `OWNER/NAME`: a repository the OctoPage App is installed on. */
  repository: string;
  /** Default `main`. */
  branch?: string;
  /** Create the database if the branch has none (otherwise it must exist). */
  create?: boolean;
  encryption?: 'none' | 'kms' | 'passphrase';
  passphrase?: string;
}

export interface Commit {
  commit: string;
  parent: string | null;
  time: number;
  message: string;
  /** The statements the commit ran (its changelog). */
  statements: { sql: string; params: SqlValue[] }[];
}

export interface Settings {
  /** `keep_all`, `keep_days N` or `keep_count N`. */
  retention: string;
  snapshot_days: number;
  live_limit: number;
  repository_budget: number;
  warn_days: number;
  grace_days: number;
}

export interface Stats {
  pages: number;
  page_size: number;
  live_bytes: number;
  live_limit: number;
  commits_30d: number;
  bytes_per_day: number;
  repository_bytes: number | null;
  repository_budget: number;
  /** Days until the repository reaches its budget at the current rate; null if not growing. */
  days_left: number | null;
  warn: boolean;
  measured: number;
}

export interface ClientOptions {
  /** The service, such as `https://octopage.example.com`. */
  url: string;
  /** An API key (`opk_…`), made in the dashboard. */
  apiKey: string;
  /** Per request; default 30 seconds. */
  timeoutMs?: number;
  /** Another `fetch` (for tests, or a runtime without a global one). */
  fetch?: typeof fetch;
}

/** A failed request: the HTTP status, and the API's error code and message. */
export class OctoPageError extends Error {
  constructor(
    readonly status: number,
    /** Stable: `conflict`, `busy`, `sql`, `not_found`, `locked`, `full`, `invalid`, … */
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = 'OctoPageError';
  }
}

// ------------------------------------------------------------------ values

function toBase64(bytes: Uint8Array): string {
  let text = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    text += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(text);
}

function fromBase64(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

const special = (value: number): string =>
  Number.isNaN(value) ? 'NaN' : value > 0 ? 'inf' : '-inf';

/** A parameter as the API takes it. */
export function encode(value: Param): unknown {
  if (value === null || value === undefined) return null;
  switch (typeof value) {
    case 'string':
    case 'boolean':
      return value;
    case 'number':
      return Number.isFinite(value) ? value : { $real: special(value) };
    case 'bigint':
      return { $int: value.toString() };
  }
  if (value instanceof Real) {
    const v = value.value;
    return { $real: Number.isFinite(v) ? String(v) : special(v) };
  }
  if (value instanceof Uint8Array) return { $blob: toBase64(value) };
  if (ArrayBuffer.isView(value)) {
    return { $blob: toBase64(new Uint8Array(value.buffer, value.byteOffset, value.byteLength)) };
  }
  throw new TypeError(`cannot send ${Object.prototype.toString.call(value)} as a SQL value`);
}

/** A value as the API returns it. */
export function decode(json: unknown): SqlValue {
  if (json === null || typeof json === 'number' || typeof json === 'string') return json;
  if (typeof json === 'object') {
    const tagged = json as Record<string, string>;
    if (typeof tagged.$blob === 'string') return fromBase64(tagged.$blob);
    if (typeof tagged.$int === 'string') return BigInt(tagged.$int);
    if (typeof tagged.$real === 'string') {
      const r = tagged.$real;
      return r === 'NaN' ? NaN : r === 'inf' ? Infinity : r === '-inf' ? -Infinity : Number(r);
    }
  }
  throw new OctoPageError(0, 'protocol', `unexpected value ${JSON.stringify(json)}`);
}

const params = (values?: readonly Param[]) => (values ?? []).map(encode);

function result(json: any): Result {
  return {
    columns: json.columns ?? [],
    rows: (json.rows ?? []).map((row: unknown[]) => row.map(decode)),
    changed: json.changed ?? 0,
    commit: json.commit ?? null,
  };
}

function objects<T>(r: Result): T[] {
  return r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]])) as T);
}

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Whether a transaction lost to another client's commit, and may simply run again. */
export const isConflict = (error: unknown): boolean =>
  error instanceof OctoPageError && (error.code === 'conflict' || error.code === 'busy');

// ------------------------------------------------------------------ the client

/** The service, signed in with an API key. */
export class OctoPage {
  readonly url: string;
  readonly #apiKey: string;
  readonly #timeoutMs: number;
  readonly #fetch: typeof fetch;

  constructor(options: ClientOptions) {
    if (!options?.url) throw new TypeError('OctoPage needs the service url');
    if (!options.apiKey) throw new TypeError('OctoPage needs an apiKey');
    this.url = options.url.replace(/\/+$/, '');
    this.#apiKey = options.apiKey;
    this.#timeoutMs = options.timeoutMs ?? 30_000;
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** Call the API: `method` on `path` (under the service's URL), with a JSON body. */
  async request<T = any>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = {
      authorization: `Bearer ${this.#apiKey}`,
      accept: 'application/json',
    };
    if (body !== undefined) headers['content-type'] = 'application/json';
    let response: Response;
    try {
      response = await this.#fetch(this.url + path, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(this.#timeoutMs),
      });
    } catch (error) {
      throw new OctoPageError(0, 'network', `${method} ${path}: ${(error as Error).message}`);
    }
    const text = await response.text();
    let json: any = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      // Not the API's JSON (a proxy's error page, say): reported below by status.
    }
    if (!response.ok) {
      throw new OctoPageError(
        response.status,
        json?.error?.code ?? 'http',
        json?.error?.message ?? `${method} ${path}: HTTP ${response.status}`,
      );
    }
    return json as T;
  }

  /** Who the key belongs to, and the App installations they may use. */
  me(): Promise<{ login: string; installations: { id: number; account: string }[] }> {
    return this.request('GET', '/v1/me');
  }

  /** Repositories the App is installed on, where databases can go. */
  async repositories(): Promise<{ repository: string; installation: number; private: boolean }[]> {
    return (await this.request('GET', '/v1/repositories')).repositories;
  }

  async databases(): Promise<DatabaseInfo[]> {
    return (await this.request('GET', '/v1/databases')).databases;
  }

  /** Add a database: create one on a branch, or register one that exists. */
  createDatabase(options: NewDatabase): Promise<DatabaseInfo> {
    return this.request('POST', '/v1/databases', options);
  }

  /** The database `id` (`db_…`): no request until it is used. */
  database(id: string): Database {
    return new Database(this, id);
  }

  async keys(): Promise<{ id: string; name: string; prefix: string; created: number }[]> {
    return (await this.request('GET', '/v1/keys')).keys;
  }

  /** A new API key: its secret (`key`) is in this answer only. */
  createKey(name: string): Promise<{ id: string; key: string; name: string; prefix: string }> {
    return this.request('POST', '/v1/keys', { name });
  }

  async revokeKey(id: string): Promise<void> {
    await this.request('DELETE', `/v1/keys/${encodeURIComponent(id)}`);
  }
}

/** Statements inside an interactive transaction. */
export class Transaction {
  constructor(
    private readonly db: Database,
    readonly id: string,
  ) {}

  async execute(sql: string, values?: readonly Param[]): Promise<Result> {
    const path = `${this.db.path}/transactions/${this.id}/execute`;
    return result(await this.db.client.request('POST', path, { sql, params: params(values) }));
  }

  async query<T = Row>(sql: string, values?: readonly Param[]): Promise<T[]> {
    return objects<T>(await this.execute(sql, values));
  }
}

/** A database on the service. */
export class Database {
  /** The commit this client's last write made. */
  lastCommit: string | null = null;

  constructor(
    readonly client: OctoPage,
    readonly id: string,
  ) {}

  /** @internal */
  get path(): string {
    return `/v1/databases/${encodeURIComponent(this.id)}`;
  }

  #noted(r: { commit: string | null }) {
    if (r.commit) this.lastCommit = r.commit;
  }

  /** Its repository, branch, head, page size and retention. */
  info(): Promise<DatabaseInfo> {
    return this.client.request('GET', this.path);
  }

  /** Stop serving it. The data stays in the repository. */
  async remove(): Promise<void> {
    await this.client.request('DELETE', this.path);
  }

  /** Open a passphrase-encrypted database for this session of the service. */
  async unlock(passphrase: string): Promise<void> {
    await this.client.request('POST', `${this.path}/unlock`, { passphrase });
  }

  /** Run one statement. A write outside a transaction is a commit of its own. */
  async execute(sql: string, values?: readonly Param[]): Promise<Result> {
    const r = result(
      await this.client.request('POST', `${this.path}/execute`, { sql, params: params(values) }),
    );
    this.#noted(r);
    return r;
  }

  /** Run one query; its rows as objects. `AS OF '<commit or time>'` reads the past. */
  async query<T = Row>(sql: string, values?: readonly Param[]): Promise<T[]> {
    return objects<T>(
      result(await this.client.request('POST', `${this.path}/query`, { sql, params: params(values) })),
    );
  }

  /**
   * Run statements as one transaction. The service runs them again by itself if another
   * client's commit gets in first, so they must not depend on reading values in between.
   */
  async batch(statements: readonly (string | Statement)[]): Promise<BatchResult> {
    const body = {
      statements: statements.map((s) =>
        typeof s === 'string' ? { sql: s, params: [] } : { sql: s.sql, params: params(s.params) },
      ),
    };
    const json = await this.client.request('POST', `${this.path}/batch`, body);
    const answer = { results: json.results.map(result), commit: json.commit ?? null };
    this.#noted(answer);
    return answer;
  }

  /**
   * Run `work` in an interactive transaction and commit it. If another client's commit got
   * in first, `work` runs again (up to `attempts` times), so it should do nothing else that
   * cannot be repeated. Throwing rolls the transaction back.
   */
  async transaction<T>(
    work: (tx: Transaction) => Promise<T>,
    { attempts = 5 }: { attempts?: number } = {},
  ): Promise<T> {
    for (let attempt = 1; ; attempt++) {
      const { transaction } = await this.client.request('POST', `${this.path}/transactions`);
      const tx = new Transaction(this, transaction);
      const base = `${this.path}/transactions/${transaction}`;
      let value: T;
      try {
        value = await work(tx);
      } catch (error) {
        await this.client.request('POST', `${base}/rollback`).catch(() => {});
        if (isConflict(error) && attempt < attempts) {
          await pause(25 * 2 ** attempt * Math.random());
          continue;
        }
        throw error;
      }
      try {
        const { commit } = await this.client.request('POST', `${base}/commit`);
        this.#noted({ commit });
        return value;
      } catch (error) {
        if (isConflict(error) && attempt < attempts) {
          await pause(25 * 2 ** attempt * Math.random());
          continue;
        }
        throw error;
      }
    }
  }

  /** The newest commits, each with the statements it ran. */
  async log(limit = 20): Promise<Commit[]> {
    const json = await this.client.request('GET', `${this.path}/log?limit=${limit}`);
    return json.commits.map((c: any) => ({
      ...c,
      statements: c.statements.map((s: any) => ({ sql: s.sql, params: s.params.map(decode) })),
    }));
  }

  async branches(): Promise<{ name: string; head: string }[]> {
    return (await this.client.request('GET', `${this.path}/branches`)).branches;
  }

  /** A new branch, from this one's head (or from branch `from`). */
  createBranch(name: string, from?: string): Promise<{ name: string; head: string }> {
    return this.client.request('POST', `${this.path}/branches`, { name, from });
  }

  async dropBranch(name: string): Promise<void> {
    await this.client.request('DELETE', `${this.path}/branches/${encodeURIComponent(name)}`);
  }

  /** Replay branch `from`'s new commits onto this one. */
  merge(from: string): Promise<{ merged: { from: string; commit: string }[]; skipped: number }> {
    return this.client.request('POST', `${this.path}/merge`, { from });
  }

  /** Size, growth, and days until the repository reaches its budget. */
  stats(): Promise<Stats> {
    return this.client.request('GET', `${this.path}/stats`);
  }

  settings(): Promise<Settings> {
    return this.client.request('GET', `${this.path}/settings`);
  }

  /** Change some settings; the rest stay. */
  async updateSettings(changes: Partial<Settings>): Promise<Settings> {
    return (await this.client.request('PUT', `${this.path}/settings`, changes)).settings;
  }

  /** Commit the maintenance workflow to the database's repository. */
  installMaintenance(options: { cliSource?: string; cliRef?: string } = {}): Promise<{ path: string; commit: string }> {
    return this.client.request('POST', `${this.path}/maintenance`, {
      cli_source: options.cliSource,
      cli_ref: options.cliRef,
    });
  }
}
