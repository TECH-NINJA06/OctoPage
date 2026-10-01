#ifndef OCTOPAGE_H
#define OCTOPAGE_H

/* Generated from crates/octopage-ffi/src/lib.rs by cbindgen when the crate builds: do not edit. */

#include <stddef.h>
#include <stdint.h>

// `octo_open` flag: create the database if the branch has none.
#define OCTO_OPEN_CREATE 1

// `octo_open` flag: when creating, create it encrypted (the `passphrase` option is required);
// its recovery key is then available once from `octo_database_recovery_key`.
#define OCTO_OPEN_ENCRYPT 2

// `OctoValue` type: SQL NULL.
#define OCTO_NULL 0

// `OctoValue` type: a 64-bit signed integer, in `integer`.
#define OCTO_INTEGER 1

// `OctoValue` type: a 64-bit float, in `real`.
#define OCTO_REAL 2

// `OctoValue` type: UTF-8 text, `len` bytes at `bytes` (OctoPage's text is also followed by a
// NUL, not counted in `len`).
#define OCTO_TEXT 3

// `OctoValue` type: a blob, `len` bytes at `bytes`.
#define OCTO_BLOB 4

// A connection: statements, queries and transactions.
typedef struct OctoConnection OctoConnection;

// An open database.
typedef struct OctoDatabase OctoDatabase;

// Options for `octo_open`, set as key and value strings (`octo_options_set`).
typedef struct OctoOptions OctoOptions;

// A query's result.
typedef struct OctoRows OctoRows;

// A status code: `OCTO_OK`, or what kind of failure (see `octo_last_error()` for the message).
typedef int32_t OctoStatus;

// A SQL value: a parameter going in, or a result coming out. The pointer in a result value
// stays valid until its `OctoRows` is freed.
typedef struct {
  // `OCTO_NULL`, `OCTO_INTEGER`, `OCTO_REAL`, `OCTO_TEXT` or `OCTO_BLOB`.
  uint32_t kind;
  // The value of an `OCTO_INTEGER`.
  int64_t integer;
  // The value of an `OCTO_REAL`.
  double real;
  // The bytes of an `OCTO_TEXT` or `OCTO_BLOB` (may be NULL when `len` is 0).
  const uint8_t *bytes;
  // How many bytes.
  size_t len;
} OctoValue;

// Success.
#define OCTO_OK 0

// A failure without a more specific code.
#define OCTO_ERROR 1

// SQLite refused the statement: syntax, a constraint, a type.
#define OCTO_SQL 2

// The commit was refused because another client changed the same data first. Nothing was
// applied; run the transaction again.
#define OCTO_CONFLICT 3

// The transaction lost too many races in a row to other writers; try again later.
#define OCTO_BUSY 4

// The repository could not be reached, or throttled the request. Nothing was committed; try
// again later.
#define OCTO_UNAVAILABLE 5

// The network failed while committing: whether the commit landed is unknown. Check before
// running it again.
#define OCTO_OUTCOME_UNKNOWN 6

// The write would grow the database past its live-size limit.
#define OCTO_FULL 7

// The database is encrypted: open it with its passphrase (or recovery key), which this was not.
#define OCTO_LOCKED 8

// There is no database on that branch, or no such repository.
#define OCTO_NOT_FOUND 9

// An argument was missing or malformed.
#define OCTO_INVALID 10

// GitHub's push protection refused the commit: something in it looks like a secret.
#define OCTO_SECRET_BLOCKED 11

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// OctoPage's version, such as "0.1.0". Belongs to OctoPage.
const char *octo_version(void);

// The message of the last failure on this thread ("" after a success). It stays valid until
// the next OctoPage call on this thread.
const char *octo_last_error(void);

// Free a string OctoPage returned as `char *`. NULL is ignored.
void octo_string_free(char *string);

// New options, all unset.
OctoOptions *octo_options_new(void);

// Set an option: "branch" (the ref, default "refs/heads/main"), "passphrase",
// "recovery_key", "cache_dir" (keep fetched pages there between runs), "user" (the user name
// for a URL remote, default "x-access-token") or "page_size" (for a new database: 4096, 8192
// or 16384). A NULL value unsets it.
OctoStatus octo_options_set(OctoOptions *options, const char *key, const char *value);

// Free options. NULL is ignored.
void octo_options_free(OctoOptions *options);

// Open the database at `location`: "OWNER/NAME" on github.com, the URL of any smart-HTTP git
// remote, or ":memory:" for a scratch database that lives in this process. `token` signs in
// (NULL: anonymous). `options` may be NULL. `flags`: `OCTO_OPEN_CREATE`, `OCTO_OPEN_ENCRYPT`.
// On success `*out` is the database; free it with `octo_database_free`.
OctoStatus octo_open(const char *location,
                     const char *token,
                     const OctoOptions *options,
                     uint32_t flags,
                     OctoDatabase **out);

// The recovery key of a database this handle just created encrypted, once: a string to free
// with `octo_string_free`, or NULL. Store it: it opens the database if the passphrase is lost.
char *octo_database_recovery_key(const OctoDatabase *db);

// Read the head now, and write its commit id (40 hex digits and a NUL) into `out`, 41 bytes.
OctoStatus octo_database_head(const OctoDatabase *db, char *out);

// Where the database lives now (it can move to a new generation): a string to free with
// `octo_string_free`, or NULL if the transport cannot say.
char *octo_database_location(const OctoDatabase *db);

// Close a database. Connections to it must be freed first. NULL is ignored.
void octo_database_free(OctoDatabase *db);

// Open a connection. Free it with `octo_connection_free`.
OctoStatus octo_connect(const OctoDatabase *db, OctoConnection **out);

// Close a connection (rolling back a transaction left open). NULL is ignored.
void octo_connection_free(OctoConnection *conn);

// Run one statement with `count` parameters (`params` may be NULL when `count` is 0). Outside
// `BEGIN … COMMIT` it commits on its own, and runs again by itself after a refused commit.
// `changed` (may be NULL) receives the rows it inserted, updated or deleted.
OctoStatus octo_execute(OctoConnection *conn,
                        const char *sql,
                        const OctoValue *params,
                        size_t count,
                        uint64_t *changed);

// Run several statements separated by semicolons, without parameters.
OctoStatus octo_execute_batch(OctoConnection *conn, const char *sql);

// Run a query (a statement that returns rows; `… AS OF '<commit or time>'` reads the past).
// On success `*out` holds the rows; free them with `octo_rows_free`.
OctoStatus octo_query(OctoConnection *conn,
                      const char *sql,
                      const OctoValue *params,
                      size_t count,
                      OctoRows **out);

// Run `count` statements as one transaction, running them all again after a refused commit
// (the way to write when others write too). Statements take no parameters here.
OctoStatus octo_run_transaction(OctoConnection *conn, const char *const *statements, size_t count);

// Whether the connection is inside `BEGIN … COMMIT`: 1 or 0.
int octo_in_transaction(const OctoConnection *conn);

// The commit this connection's last commit made (40 hex digits and a NUL into `out`, 41
// bytes). `OCTO_NOT_FOUND` if it has not committed.
OctoStatus octo_last_commit(const OctoConnection *conn, char *out);

// How many rows.
size_t octo_rows_count(const OctoRows *rows);

// How many columns.
size_t octo_rows_columns(const OctoRows *rows);

// Column `column`'s name, or NULL. Belongs to the rows.
const char *octo_rows_column_name(const OctoRows *rows, size_t column);

// The value at `row` and `column`. Its bytes belong to the rows.
OctoStatus octo_rows_value(const OctoRows *rows, size_t row, size_t column, OctoValue *out);

// Free rows. NULL is ignored.
void octo_rows_free(OctoRows *rows);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* OCTOPAGE_H */
