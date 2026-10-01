import base64
import json
import math
import random
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, TypeVar, Union

__version__ = "0.1.0"
__all__ = [
    "Client", "Database", "Transaction", "Result", "BatchResult", "Error", "ConflictError",
    "encode", "decode",
]

EXACT = 2 ** 53
T = TypeVar("T")


class Error(Exception):
    """A failed request: the HTTP status, and the API's error code and message."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(message)
        self.status = status
        #: Stable: ``conflict``, ``busy``, ``sql``, ``not_found``, ``locked``, ``full``, …
        self.code = code
        self.message = message

    def __repr__(self) -> str:
        return f"{type(self).__name__}({self.status}, {self.code!r}, {self.message!r})"


class ConflictError(Error):
    """Another client's commit got in first; the transaction can simply run again."""


# ------------------------------------------------------------------ values

def encode(value: Any) -> Any:
    """A parameter as the API takes it."""
    if value is None or isinstance(value, (str, bool)):
        return value
    if isinstance(value, int):
        return value if -EXACT <= value <= EXACT else {"$int": str(value)}
    if isinstance(value, float):
        if math.isfinite(value):
            return value
        return {"$real": "NaN" if math.isnan(value) else ("inf" if value > 0 else "-inf")}
    if isinstance(value, (bytes, bytearray, memoryview)):
        return {"$blob": base64.b64encode(bytes(value)).decode("ascii")}
    raise TypeError(f"cannot send {type(value).__name__} as a SQL value")


def decode(value: Any) -> Any:
    """A value as the API returns it."""
    if isinstance(value, dict):
        if "$blob" in value:
            return base64.b64decode(value["$blob"])
        if "$int" in value:
            return int(value["$int"])
        if "$real" in value:
            special = {"NaN": math.nan, "inf": math.inf, "-inf": -math.inf}
            text = value["$real"]
            return special[text] if text in special else float(text)
    return value


def _params(values: Optional[Sequence[Any]]) -> List[Any]:
    if values is None:
        return []
    if isinstance(values, (str, bytes, dict)):
        raise TypeError("parameters are a list or tuple of values")
    return [encode(v) for v in values]


@dataclass
class Result:
    """What a statement returned."""

    columns: List[str] = field(default_factory=list)
    rows: List[tuple] = field(default_factory=list)
    #: Rows the statement inserted, updated or deleted.
    changed: int = 0
    #: The commit a write made (a git commit id), or None.
    commit: Optional[str] = None

    def dicts(self) -> List[Dict[str, Any]]:
        """The rows as dicts, by column name."""
        return [dict(zip(self.columns, row)) for row in self.rows]


@dataclass
class BatchResult:
    results: List[Result]
    commit: Optional[str]


def _result(body: Dict[str, Any]) -> Result:
    return Result(
        columns=body.get("columns") or [],
        rows=[tuple(decode(v) for v in row) for row in body.get("rows") or []],
        changed=body.get("changed") or 0,
        commit=body.get("commit"),
    )


# ------------------------------------------------------------------ the client

class Client:
    """The service, signed in with an API key (``opk_…``, made in the dashboard)."""

    def __init__(self, url: str, api_key: str, *, timeout: float = 30.0):
        if not url:
            raise ValueError("Client needs the service url")
        if not api_key:
            raise ValueError("Client needs an api_key")
        self.url = url.rstrip("/")
        self._api_key = api_key
        self.timeout = timeout

    def request(self, method: str, path: str, body: Any = None) -> Any:
        """Call the API: ``method`` on ``path`` (under the service's URL), with a JSON body."""
        data = None
        headers = {"Authorization": f"Bearer {self._api_key}", "Accept": "application/json",
                   "User-Agent": f"octopage-client-python/{__version__}"}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self.url + path, data=data, headers=headers,
                                         method=method)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                text = response.read().decode("utf-8")
                return json.loads(text) if text else None
        except urllib.error.HTTPError as error:
            text = error.read().decode("utf-8", "replace")
            try:
                detail = json.loads(text)["error"]
                code, message = detail["code"], detail["message"]
            except (ValueError, KeyError, TypeError):
                code, message = "http", f"{method} {path}: HTTP {error.code}"
            kind = ConflictError if code in ("conflict", "busy") else Error
            raise kind(error.code, code, message) from None
        except urllib.error.URLError as error:
            raise Error(0, "network", f"{method} {path}: {error.reason}") from None

    def me(self) -> Dict[str, Any]:
        """Who the key belongs to, and the App installations they may use."""
        return self.request("GET", "/v1/me")

    def repositories(self) -> List[Dict[str, Any]]:
        """Repositories the App is installed on, where databases can go."""
        return self.request("GET", "/v1/repositories")["repositories"]

    def databases(self) -> List[Dict[str, Any]]:
        return self.request("GET", "/v1/databases")["databases"]

    def create_database(self, repository: str, *, branch: str = "main", create: bool = True,
                        encryption: Optional[str] = None,
                        passphrase: Optional[str] = None) -> Dict[str, Any]:
        """Add a database: create one on ``branch`` of ``repository`` (``OWNER/NAME``), or
        register one that exists. The answer's ``new`` says which; an encrypted database's
        ``recovery_key`` is in it once only."""
        body = {"repository": repository, "branch": branch, "create": create}
        if encryption is not None:
            body["encryption"] = encryption
        if passphrase is not None:
            body["passphrase"] = passphrase
        return self.request("POST", "/v1/databases", body)

    def database(self, id: str) -> "Database":
        """The database ``id`` (``db_…``): no request until it is used."""
        return Database(self, id)

    def keys(self) -> List[Dict[str, Any]]:
        return self.request("GET", "/v1/keys")["keys"]

    def create_key(self, name: str) -> Dict[str, Any]:
        """A new API key: its secret (``key``) is in this answer only."""
        return self.request("POST", "/v1/keys", {"name": name})

    def revoke_key(self, id: str) -> None:
        self.request("DELETE", f"/v1/keys/{urllib.parse.quote(id, safe='')}")


class Transaction:
    """Statements inside an interactive transaction."""

    def __init__(self, db: "Database", id: str):
        self.db = db
        self.id = id

    def execute(self, sql: str, params: Optional[Sequence[Any]] = None) -> Result:
        path = f"{self.db.path}/transactions/{self.id}/execute"
        return _result(self.db.client.request("POST", path, {"sql": sql, "params": _params(params)}))

    def query(self, sql: str, params: Optional[Sequence[Any]] = None) -> List[Dict[str, Any]]:
        return self.execute(sql, params).dicts()


class Database:
    """A database on the service."""

    def __init__(self, client: Client, id: str):
        self.client = client
        self.id = id
        #: The commit this client's last write made.
        self.last_commit: Optional[str] = None

    @property
    def path(self) -> str:
        return f"/v1/databases/{urllib.parse.quote(self.id, safe='')}"

    def _noted(self, commit: Optional[str]) -> None:
        if commit:
            self.last_commit = commit

    def info(self) -> Dict[str, Any]:
        """Its repository, branch, head, page size and retention."""
        return self.client.request("GET", self.path)

    def remove(self) -> None:
        """Stop serving it. The data stays in the repository."""
        self.client.request("DELETE", self.path)

    def unlock(self, passphrase: str) -> None:
        """Open a passphrase-encrypted database for this session of the service."""
        self.client.request("POST", f"{self.path}/unlock", {"passphrase": passphrase})

    def execute(self, sql: str, params: Optional[Sequence[Any]] = None) -> Result:
        """Run one statement. A write outside a transaction is a commit of its own."""
        result = _result(self.client.request(
            "POST", f"{self.path}/execute", {"sql": sql, "params": _params(params)}))
        self._noted(result.commit)
        return result

    def query(self, sql: str, params: Optional[Sequence[Any]] = None) -> List[Dict[str, Any]]:
        """Run one query; its rows as dicts. ``AS OF '<commit or time>'`` reads the past."""
        return _result(self.client.request(
            "POST", f"{self.path}/query", {"sql": sql, "params": _params(params)})).dicts()

    def batch(self, statements: Iterable[Union[str, Sequence[Any]]]) -> BatchResult:
        """Run statements as one transaction: each a string, or ``(sql, params)``. The service
        runs them again by itself if another client's commit gets in first."""
        body = []
        for statement in statements:
            if isinstance(statement, str):
                body.append({"sql": statement, "params": []})
            else:
                sql, params = statement
                body.append({"sql": sql, "params": _params(params)})
        answer = self.client.request("POST", f"{self.path}/batch", {"statements": body})
        batch = BatchResult([_result(r) for r in answer["results"]], answer.get("commit"))
        self._noted(batch.commit)
        return batch

    def transaction(self, work: Callable[[Transaction], T], *, attempts: int = 5) -> T:
        """Run ``work(tx)`` in an interactive transaction and commit it; its return value.
        If another client's commit got in first, ``work`` runs again (up to ``attempts``
        times), so it should do nothing else that cannot be repeated. An exception rolls the
        transaction back."""
        for attempt in range(1, attempts + 1):
            answer = self.client.request("POST", f"{self.path}/transactions")
            tx = Transaction(self, answer["transaction"])
            base = f"{self.path}/transactions/{tx.id}"
            try:
                value = work(tx)
            except BaseException as error:
                try:
                    self.client.request("POST", f"{base}/rollback")
                except Error:
                    pass
                if isinstance(error, ConflictError) and attempt < attempts:
                    time.sleep(0.025 * 2 ** attempt * random.random())
                    continue
                raise
            try:
                self._noted(self.client.request("POST", f"{base}/commit").get("commit"))
                return value
            except ConflictError:
                if attempt == attempts:
                    raise
                time.sleep(0.025 * 2 ** attempt * random.random())
        raise AssertionError("unreachable")

    def log(self, limit: int = 20) -> List[Dict[str, Any]]:
        """The newest commits, each with the statements it ran."""
        commits = self.client.request("GET", f"{self.path}/log?limit={int(limit)}")["commits"]
        for commit in commits:
            for statement in commit["statements"]:
                statement["params"] = [decode(p) for p in statement["params"]]
        return commits

    def branches(self) -> List[Dict[str, str]]:
        return self.client.request("GET", f"{self.path}/branches")["branches"]

    def create_branch(self, name: str, from_: Optional[str] = None) -> Dict[str, str]:
        """A new branch, from this one's head (or from branch ``from_``)."""
        return self.client.request("POST", f"{self.path}/branches", {"name": name, "from": from_})

    def drop_branch(self, name: str) -> None:
        self.client.request(
            "DELETE", f"{self.path}/branches/{urllib.parse.quote(name, safe='')}")

    def merge(self, from_: str) -> Dict[str, Any]:
        """Replay branch ``from_``'s new commits onto this one."""
        return self.client.request("POST", f"{self.path}/merge", {"from": from_})

    def stats(self) -> Dict[str, Any]:
        """Size, growth, and days until the repository reaches its budget."""
        return self.client.request("GET", f"{self.path}/stats")

    def settings(self) -> Dict[str, Any]:
        return self.client.request("GET", f"{self.path}/settings")

    def update_settings(self, **changes: Any) -> Dict[str, Any]:
        """Change some settings (``retention="keep_days 30"``, ``live_limit=…``); the rest stay."""
        return self.client.request("PUT", f"{self.path}/settings", changes)["settings"]

    def install_maintenance(self, cli_source: Optional[str] = None,
                            cli_ref: Optional[str] = None) -> Dict[str, str]:
        """Commit the maintenance workflow to the database's repository."""
        return self.client.request("POST", f"{self.path}/maintenance",
                                   {"cli_source": cli_source, "cli_ref": cli_ref})
