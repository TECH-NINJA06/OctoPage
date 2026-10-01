/** A SQL value going in: integers beyond 2^53 as bigints, blobs as Buffers or typed arrays. */
export type Param = null | undefined | boolean | number | bigint | string | Buffer | Uint8Array;
/** A SQL value coming out. */
export type Cell = null | number | bigint | string | Buffer;

export interface OpenOptions {
  /** A fine-grained token or an app installation token. */
  token?: string;
  /** The branch holding the database (default "main"). */
  branch?: string;
  passphrase?: string;
  recoveryKey?: string;
  /** Create the database if the branch has none. */
  create?: boolean;
  /** When creating, encrypt it with `passphrase`. */
  encrypt?: boolean;
  /** Keep fetched pages here between runs. */
  cacheDir?: string;
  /** The user to sign in as at a URL (default "x-access-token"). */
  user?: string;
  /** A new database's page size: 4096, 8192 or 16384. */
  pageSize?: number;
}

export interface Outcome {
  columns: string[];
  rows: Cell[][];
  /** Rows inserted, updated or deleted. */
  changed: number;
  lastInsertRowid: number;
  /** The commit this statement made, if it made one. */
  commit: string | null;
}

export interface BatchOutcome {
  changed: number[];
  commit: string | null;
}

/** Errors carry `code`: "sql", "constraint", "conflict", "busy", "unavailable",
 *  "outcome_unknown", "full", "secret_blocked", "locked", "not_found", "invalid", "closed". */
export interface OctoPageError extends Error {
  code: string;
}

export class Connection {
  execute(sql: string, params?: Param[]): Promise<Outcome>;
  query(sql: string, params?: Param[]): Promise<Record<string, Cell>[]>;
  executeBatch(sql: string): Promise<void>;
  batch(statements: (string | { sql: string; params?: Param[] })[]): Promise<BatchOutcome>;
  transaction<T>(body: (conn: Connection) => Promise<T>, options?: { attempts?: number }): Promise<T>;
  readonly inTransaction: boolean;
  close(): void;
}

export class Database {
  static open(location: string, options?: OpenOptions): Promise<Database>;
  connect(options?: { bigints?: boolean }): Promise<Connection>;
  head(): Promise<string>;
  readonly location: string | null;
  readonly recoveryKey: string | null;
}

export function version(): string;
