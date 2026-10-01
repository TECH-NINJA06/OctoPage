"""OctoPage for Python: a relational database kept in a GitHub repository.

Every 4 KB page is a git blob, every committed transaction a git commit. The interface follows
the DB-API 2.0 (PEP 249)::

    import octopage

    conn = octopage.connect("OWNER/NAME", token, create=True)
    conn.execute("CREATE TABLE IF NOT EXISTS notes(id INTEGER PRIMARY KEY, body TEXT)")
    conn.execute("INSERT INTO notes(body) VALUES(?)", ("hello",))
    conn.commit()                      # one git commit
    for row in conn.execute("SELECT * FROM notes"):
        print(row)

``commit()`` raises ``ConflictError`` when another client changed the same data first:
nothing was applied, so run the transaction again (``run_transaction`` does that by itself).
``SELECT … AS OF '<commit or time>'`` reads the database as it was then.
"""

from ._octopage import (  # noqa: F401
    Connection,
    Cursor,
    DatabaseError,
    DataError,
    ConflictError,
    Error,
    FullError,
    IntegrityError,
    InterfaceError,
    InternalError,
    LockedError,
    NotSupportedError,
    OperationalError,
    OutcomeUnknownError,
    ProgrammingError,
    UnavailableError,
    Warning,
    __version__,
    apilevel,
    connect,
    paramstyle,
    threadsafety,
)

__all__ = [
    "connect",
    "Connection",
    "Cursor",
    "apilevel",
    "threadsafety",
    "paramstyle",
    "Warning",
    "Error",
    "InterfaceError",
    "DatabaseError",
    "DataError",
    "OperationalError",
    "IntegrityError",
    "InternalError",
    "ProgrammingError",
    "NotSupportedError",
    "ConflictError",
    "UnavailableError",
    "OutcomeUnknownError",
    "FullError",
    "LockedError",
]
