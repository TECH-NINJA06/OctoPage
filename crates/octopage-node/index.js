'use strict';
// OctoPage for Node.js: the native addon (octopage.node), and what is simpler in JavaScript —
// error codes on `err.code`, transactions that run again after a refused commit, and
// parameters normalized (undefined as null, typed arrays as Buffers).

const path = require('path');

const native = require(process.env.OCTOPAGE_NATIVE || path.join(__dirname, 'octopage.node'));

const CODE = /^\[(\w+)\] /;

/** Move the `[code]` before an error's message onto `error.code`. */
function withCode(error) {
  if (error && typeof error.message === 'string') {
    const match = CODE.exec(error.message);
    if (match) {
      error.code = match[1];
      error.message = error.message.slice(match[0].length);
    }
  }
  return error;
}

function normalize(params) {
  if (params === undefined || params === null) return undefined;
  if (!Array.isArray(params)) throw withCode(new TypeError('[invalid] params is an array'));
  return params.map((value) => {
    if (value === undefined) return null;
    if (ArrayBuffer.isView(value) && !Buffer.isBuffer(value)) {
      return Buffer.from(value.buffer, value.byteOffset, value.byteLength);
    }
    return value;
  });
}

async function call(work) {
  try {
    return await work();
  } catch (error) {
    throw withCode(error);
  }
}

class Connection {
  constructor(inner) {
    this._inner = inner;
    // A connection holds one transaction at a time: transaction() calls wait their turn.
    this._turn = Promise.resolve();
  }

  /** Run one statement: `{columns, rows, changed, lastInsertRowid, commit}`. */
  execute(sql, params) {
    return call(() => this._inner.execute(sql, normalize(params)));
  }

  /** Run several statements separated by semicolons, without parameters. */
  executeBatch(sql) {
    return call(() => this._inner.executeBatch(sql));
  }

  /** Run a query: its rows as objects keyed by column. */
  async query(sql, params) {
    const { columns, rows } = await this.execute(sql, params);
    return rows.map((row) => Object.fromEntries(columns.map((c, i) => [c, row[i]])));
  }

  /** Run statements (strings or `{sql, params}`) as one transaction, run again by itself
   *  after a refused commit. */
  batch(statements) {
    return call(() =>
      this._inner.batch(
        statements.map((s) =>
          typeof s === 'string' ? { sql: s } : { sql: s.sql, params: normalize(s.params) },
        ),
      ),
    );
  }

  /** Run `body(connection)` in a transaction and commit it; when another client changed
   *  the same data first, roll back and run it again (up to `attempts` times). Transactions
   *  on one connection run one after another; use more connections to run them side by side. */
  transaction(body, options) {
    const run = this._turn.then(() => this._transaction(body, options));
    this._turn = run.catch(() => {});
    return run;
  }

  async _transaction(body, { attempts = 8 } = {}) {
    for (let attempt = 1; ; attempt++) {
      await this.execute('BEGIN');
      try {
        const result = await body(this);
        await this.execute('COMMIT');
        return result;
      } catch (error) {
        if (this.inTransaction) {
          await this.execute('ROLLBACK').catch(() => {});
        }
        if (error.code === 'conflict' && attempt < attempts) continue;
        throw error;
      }
    }
  }

  get inTransaction() {
    return this._inner.inTransaction;
  }

  close() {
    this._inner.close();
  }
}

class Database {
  constructor(inner) {
    this._inner = inner;
  }

  /** Open the database at `location`: "OWNER/NAME" on github.com, a git remote's URL, or
   *  ":memory:". */
  static async open(location, options = {}) {
    return new Database(await call(() => native.Database.open(location, options)));
  }

  /** A new connection; `{bigints: true}` returns every integer as a bigint. */
  async connect(options = {}) {
    return new Connection(await call(() => this._inner.connect(Boolean(options.bigints))));
  }

  /** Read the head now: its commit id. */
  head() {
    return call(() => this._inner.head());
  }

  /** Where the database lives now (OWNER/NAME on GitHub). */
  get location() {
    return this._inner.location;
  }

  /** The recovery key of a database just created encrypted: once, then null. */
  get recoveryKey() {
    return this._inner.recoveryKey;
  }
}

module.exports = { Database, Connection, version: native.version };
