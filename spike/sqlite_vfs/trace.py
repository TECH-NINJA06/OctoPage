import ctypes
import json
import os
import random
import shutil
import statistics
import string
import sys
import tempfile

import apsw

PAGE_SIZE = 4096
RESERVED = 32  # room for OctoPage's page header
ROWS_T = 100_000
ROWS_U = 10_000

# Page 1 header fields (https://www.sqlite.org/fileformat.html#the_database_header)
HEADER_FIELDS = {
    (24, 28): "change counter",
    (28, 32): "database size",
    (32, 36): "freelist trunk",
    (36, 40): "freelist count",
    (40, 44): "schema cookie",
    (92, 96): "version-valid-for",
    (96, 100): "sqlite version",
}
# Fields a VFS can derive at commit time, as OctoPage derives its superblock. The database size
# is the highest page in the page map; the VFS knows it.
DERIVABLE = {"change counter", "version-valid-for", "sqlite version", "database size"}


class TraceVFS(apsw.VFS):
    def __init__(self):
        self.reads = set()
        self.writes = set()
        self.page1_fields = set()
        self.page1_body = False
        super().__init__("trace", "")

    def reset(self):
        self.reads, self.writes, self.page1_fields, self.page1_body = set(), set(), set(), False

    def xOpen(self, name, flags):
        return TraceFile(self, name, flags)


class TraceFile(apsw.VFSFile):
    def __init__(self, vfs, name, flags):
        self.vfs = vfs
        self.main = bool(flags[0] & apsw.mapping_open_flags["SQLITE_OPEN_MAIN_DB"])
        super().__init__("", name, flags)

    def xRead(self, amount, offset):
        data = super().xRead(amount, offset)
        if self.main:
            first, last = offset // PAGE_SIZE + 1, (offset + max(amount, 1) - 1) // PAGE_SIZE + 1
            self.vfs.reads.update(range(first, last + 1))
        return data

    def xWrite(self, data, offset):
        if self.main:
            page = offset // PAGE_SIZE + 1
            self.vfs.writes.add(page)
            if page == 1:
                try:
                    old = super().xRead(len(data), offset)
                except Exception:  # a read past the end of a growing file
                    old = b""
                if len(old) == len(data):
                    for (start, end), field in HEADER_FIELDS.items():
                        if old[start:end] != data[start:end]:
                            self.vfs.page1_fields.add(field)
                    # Anything beyond the 100-byte header is the sqlite_schema table's root page.
                    if old[100:] != data[100:]:
                        self.vfs.page1_body = True
                else:
                    self.vfs.page1_body = True
        super().xWrite(data, offset)


VFS = TraceVFS()


def connect(path):
    con = apsw.Connection(path, vfs="trace")
    con.execute("PRAGMA journal_mode=MEMORY")  # atomicity would come from the OctoPage commit
    return con


def words(n):
    return "".join(random.choices(string.ascii_letters, k=n))


def build(path, steady):
    """`steady`: rows arrive in random order and nothing is vacuumed, so pages are about as full as
    in a database that has lived for a while. Otherwise the file is vacuumed: every page 100% full."""
    con = apsw.Connection(path)
    con.execute(f"PRAGMA page_size={PAGE_SIZE}")
    # Reserve bytes at the end of every page (the hook SQLCipher uses for nonces and tags).
    reserve = ctypes.c_int(RESERVED)
    con.file_control("main", apsw.SQLITE_FCNTL_RESERVE_BYTES, ctypes.addressof(reserve))
    con.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT)")
    con.execute("CREATE INDEX t_a ON t(a)")
    con.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, v TEXT)")
    ids_t = list(range(1, ROWS_T + 1))
    ids_u = list(range(1, ROWS_U + 1))
    if steady:
        random.shuffle(ids_t)
        random.shuffle(ids_u)
    with con:
        con.executemany("INSERT INTO t VALUES(?,?,?)", ((i * 2, random.randrange(10**9), words(40)) for i in ids_t))
        con.executemany("INSERT INTO u VALUES(?,?)", ((i, words(60)) for i in ids_u))
    if not steady:
        con.execute("VACUUM")
    con.close()
    with open(path, "rb") as f:
        header = f.read(100)
    return header[20], os.path.getsize(path) // PAGE_SIZE


def run(path, sql, params=(), write=True):
    """Run one statement in its own transaction on a fresh connection (cold page cache)."""
    con = connect(path)
    VFS.reset()
    con.execute("BEGIN IMMEDIATE" if write else "BEGIN")
    for _ in con.execute(sql, params):
        pass
    con.execute("COMMIT")
    con.close()
    return {
        "reads": set(VFS.reads),
        "writes": set(VFS.writes),
        "page1_fields": set(VFS.page1_fields),
        "page1_body": VFS.page1_body,
    }


def max_id(path, table):
    con = apsw.Connection(path)
    (value,) = con.execute(f"SELECT max(id) FROM {table}").fetchone()
    con.close()
    return value


OPERATIONS = {
    "point read by primary key": ("SELECT * FROM t WHERE id=?", lambda p: (random.randrange(1, ROWS_T) * 2,), False),
    "point read through index": ("SELECT * FROM t WHERE a=?", lambda p: (random.randrange(10**9),), False),
    "range scan, 1,000 rows": ("SELECT count(*), sum(a) FROM t WHERE id BETWEEN ? AND ?+1998", lambda p: (lambda s: (s, s))(random.randrange(1, ROWS_T - 1000) * 2), False),
    "update a row by primary key": ("UPDATE t SET b=? WHERE id=?", lambda p: (words(40), random.randrange(1, ROWS_T) * 2), True),
    "update an indexed column": ("UPDATE t SET a=? WHERE id=?", lambda p: (random.randrange(10**9), random.randrange(1, ROWS_T) * 2), True),
    "insert at the end (next id)": ("INSERT INTO t VALUES(?,?,?)", lambda p: (max_id(p, "t") + 2, random.randrange(10**9), words(40)), True),
    "insert in the middle": ("INSERT INTO t VALUES(?,?,?)", lambda p: (random.randrange(1, ROWS_T) * 2 + 1, random.randrange(10**9), words(40)), True),
    "delete a row": ("DELETE FROM t WHERE id=?", lambda p: (random.randrange(1, ROWS_T) * 2,), True),
    "add a column (DDL)": ("ALTER TABLE u ADD COLUMN c{n} INTEGER", None, True),
}


def measure_operations(base, workdir, repeats=20):
    rows = []
    for name, (sql, params, write) in OPERATIONS.items():
        reads, writes, page1_writes, fields, body = [], [], 0, set(), 0
        for i in range(repeats):
            path = os.path.join(workdir, "op.db")
            shutil.copyfile(base, path)
            statement = sql.format(n=i) if params is None else sql
            r = run(path, statement, () if params is None else params(path), write)
            reads.append(len(r["reads"]))
            writes.append(len(r["writes"]))
            page1_writes += 1 in r["writes"]
            fields |= r["page1_fields"]
            body += r["page1_body"]
        rows.append({
            "operation": name,
            "pages_read": statistics.median(reads),
            "pages_written": statistics.median(writes),
            "page1_written": f"{page1_writes}/{repeats}",
            "page1_fields": sorted(fields),
            "page1_body_changed": body,
        })
    return rows


PAIRS = {
    "update two random rows of t": ("update a row by primary key", "update a row by primary key"),
    "update a row of t / a row of u": ("update a row by primary key", "update a row of u"),
    "two inserts at the end of t": ("insert at the end (next id)", "insert at the end (next id)"),
    "insert at the end of t / of u": ("insert at the end (next id)", "insert at the end of u"),
    "update indexed column, two rows": ("update an indexed column", "update an indexed column"),
    "read a row / update another row": ("point read by primary key", "update a row by primary key"),
}

EXTRA = {
    "update a row of u": ("UPDATE u SET v=? WHERE id=?", lambda p: (words(60), random.randrange(1, ROWS_U)), True),
    "insert at the end of u": ("INSERT INTO u VALUES(?,?)", lambda p: (max_id(p, "u") + 1, words(60)), True),
}


def one(path, name):
    sql, params, write = {**OPERATIONS, **EXTRA}[name]
    return run(path, sql, params(path), write)


def conflicts(first, second, derived):
    """Would `second`, started from the same state, conflict after `first` commits?"""
    changed = set(first["writes"])
    touched = second["reads"] | second["writes"]
    if derived:
        # Page 1 counts only if a non-derivable field (or the schema table) changed.
        if 1 in changed and not (first["page1_fields"] - DERIVABLE) and not first["page1_body"]:
            changed.discard(1)
        # Every transaction reads page 1 for the change counter and schema cookie; that read
        # matters only if the schema cookie changed.
        if "schema cookie" not in first["page1_fields"]:
            touched.discard(1)
    return bool(changed & touched)


def measure_pairs(base, workdir, repeats=50):
    rows = []
    for name, (first_op, second_op) in PAIRS.items():
        strict = derived = 0
        for _ in range(repeats):
            a, b = os.path.join(workdir, "a.db"), os.path.join(workdir, "b.db")
            shutil.copyfile(base, a)
            shutil.copyfile(base, b)
            ra, rb = one(a, first_op), one(b, second_op)
            if first_op.startswith("point read"):
                ra, rb = rb, ra  # the reader loses if the writer commits first
            strict += conflicts(ra, rb, False)
            derived += conflicts(ra, rb, True)
        rows.append({"pair": name, "conflict_strict": strict / repeats, "conflict_page1_derived": derived / repeats})
    return rows


def main():
    random.seed(7)
    workdir = tempfile.mkdtemp(prefix="octopage-sqlite-")
    try:
        base = os.path.join(workdir, "base.db")
        steady = "--steady" in sys.argv
        reserved, pages = build(base, steady)
        ops = measure_operations(base, workdir)
        pairs = measure_pairs(base, workdir)
        result = {"sqlite": apsw.sqlite_lib_version(), "page_size": PAGE_SIZE, "reserved_bytes": reserved, "pages": pages, "operations": ops, "pairs": pairs}
        if "--json" in sys.argv:
            print(json.dumps(result, indent=1))
            return
        kind = "steady state (random insertion order)" if steady else "freshly vacuumed (pages 100% full)"
        print(f"SQLite {result['sqlite']}, page size {PAGE_SIZE}, reserved bytes per page: {reserved}, {pages} pages, {kind}\n")
        print("| Operation (own transaction, cold cache) | Pages read | Pages written | Page 1 written | Page 1 fields changed |")
        print("|---|---|---|---|---|")
        for r in ops:
            fields = ", ".join(r["page1_fields"]) + (" + schema table" if r["page1_body_changed"] else "")
            print(f"| {r['operation']} | {r['pages_read']:g} | {r['pages_written']:g} | {r['page1_written']} | {fields or '-'} |")
        print("\n| Two transactions from the same state | Conflict rate (strict) | Conflict rate (page 1 bookkeeping derived) |")
        print("|---|---|---|")
        for r in pairs:
            print(f"| {r['pair']} | {r['conflict_strict']:.0%} | {r['conflict_page1_derived']:.0%} |")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
