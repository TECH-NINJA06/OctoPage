import argparse
import base64
import hashlib
import http.client
import json
import os
import platform
import random
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.parse
import zlib
from pathlib import Path

VERSION = "0.1.0"
PAGE_SIZE = 4096
DB_PAGES = 65536  # page-id space the spike writes into: pages/00/BB/CC
ZERO_SHA = "0" * 40
USER_AGENT = f"octospike/{VERSION} (OctoPage phase 0 transport spike)"
CONNECT_TIMEOUT = 3  # seconds, per resolved address

# Sample counts. Tuples are (raw, cli). Uploads about 110 MB of random pages per run.
GITHUB_REPS = {
    "push": {1: (30, 10), 100: (15, 5), 5000: (4, 1)},
    "ref_read": 30,
    "fetch": {1: (30, 10), 64: (15, 5), 512: (5, 2), 2048: (2, 0)},
    "raw_cdn": 40,
    "rest": {"get": 30, "ff": 10, "reject": 5},
    "race": {"trials": 10, "cas_exact": 2, "stale": 10},
}
SELFTEST_REPS = {
    "push": {1: (3, 2), 100: (2, 1), 5000: (1, 1)},
    "ref_read": 5,
    "fetch": {1: (3, 2), 64: (2, 1), 512: (1, 1)},
    "raw_cdn": 0,
    "rest": None,
    "race": {"trials": 3, "cas_exact": 1, "stale": 2},
}
SUITES = ["push", "ref_read", "fetch", "raw_cdn", "rest", "race", "refs"]

# Targets from the spec's transport table, for the report.
SPEC_TARGETS = {
    "push/1": "300-900 ms",
    "push/100": "300-900 ms",
    "push/5000": "3-10 s",
    "fetch": "150-400 ms",
    "ref-read/rest": "80-200 ms",
    "ref-move/rest": "80-200 ms",
    "raw-cdn/repeat": "40-120 ms",
}


class SpikeError(Exception):
    pass


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------- pkt-line

FLUSH, DELIM = b"0000", b"0001"


def pkt(data):
    if isinstance(data, str):
        data = data.encode()
    return b"%04x" % (len(data) + 4) + data


def parse_pkts(data):
    """Split a pkt-line stream. Special packets (flush, delim, response-end) come back as ints."""
    out, i = [], 0
    while i + 4 <= len(data):
        n = int(data[i:i + 4], 16)
        if n < 4:
            out.append(n)
            i += 4
            continue
        out.append(data[i + 4:i + n])
        i += n
    return out


def pkt_lines(data):
    return [p.decode("utf-8", "replace").rstrip("\n") for p in parse_pkts(data) if isinstance(p, bytes)]


# --------------------------------------------------------------------------- packfiles

TYPE_NAMES = {1: b"commit", 2: b"tree", 3: b"blob", 4: b"tag"}


def apply_delta(base, delta):
    i = 0

    def varint():
        nonlocal i
        value = shift = 0
        while True:
            c = delta[i]
            i += 1
            value |= (c & 0x7F) << shift
            shift += 7
            if not c & 0x80:
                return value

    varint()  # source size
    varint()  # target size
    out = bytearray()
    while i < len(delta):
        op = delta[i]
        i += 1
        if op & 0x80:  # copy from base
            offset = size = 0
            for bit, shift in ((0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)):
                if op & bit:
                    offset |= delta[i] << shift
                    i += 1
            for bit, shift in ((0x10, 0), (0x20, 8), (0x40, 16)):
                if op & bit:
                    size |= delta[i] << shift
                    i += 1
            out += base[offset:offset + (size or 0x10000)]
        elif op:  # insert literal
            out += delta[i:i + op]
            i += op
        else:
            raise SpikeError("invalid delta opcode 0")
    return bytes(out)


def parse_pack(pack):
    """Parse a packfile and resolve its deltas. Returns (count, [(type, bytes)], trailer_ok, n_deltas)."""
    if pack[:4] != b"PACK":
        raise SpikeError("response did not contain a packfile")
    _, count = struct.unpack(">II", pack[4:12])
    trailer_ok = hashlib.sha1(pack[:-20]).digest() == pack[-20:]
    view, i, entries = memoryview(pack), 12, {}
    for _ in range(count):
        start = i
        c = pack[i]
        i += 1
        kind, size, shift = (c >> 4) & 7, c & 15, 4
        while c & 0x80:
            c = pack[i]
            i += 1
            size |= (c & 0x7F) << shift
            shift += 7
        base = None
        if kind == 6:  # ofs-delta: base is at a negative offset from this entry
            c = pack[i]
            i += 1
            offset = c & 0x7F
            while c & 0x80:
                c = pack[i]
                i += 1
                offset = ((offset + 1) << 7) | (c & 0x7F)
            base = start - offset
        elif kind == 7:  # ref-delta: base named by object id
            base = pack[i:i + 20].hex()
            i += 20
        window = size + 256
        while True:
            d = zlib.decompressobj()
            chunk = view[i:i + window]
            data = d.decompress(chunk)
            if d.eof or i + window >= len(pack):
                break
            window *= 2
        i += len(chunk) - len(d.unused_data)
        entries[start] = (kind, data, base)

    resolved, by_sha = {}, {}

    def resolve(off):
        if off not in resolved:
            kind, data, base = entries[off]
            if kind == 6:
                base_kind, base_data = resolve(base)
                resolved[off] = (base_kind, apply_delta(base_data, data))
            elif kind == 7:
                base_kind, base_data = resolve(by_sha[base])
                resolved[off] = (base_kind, apply_delta(base_data, data))
            else:
                resolved[off] = (kind, data)
            k, d = resolved[off]
            by_sha[hashlib.sha1(TYPE_NAMES[k] + b" %d\0" % len(d) + d).hexdigest()] = off
        return resolved[off]

    pending = list(entries)
    while pending:  # ref-delta bases may appear later in the pack than the delta
        progress = [off for off in pending if entries[off][0] != 7 or entries[off][2] in by_sha]
        if not progress:
            raise SpikeError("ref-delta base missing from pack")
        for off in progress:
            resolve(off)
        pending = [off for off in pending if off not in resolved]
    n_deltas = sum(1 for kind, _, _ in entries.values() if kind in (6, 7))
    return count, [resolved[off] for off in entries], trailer_ok, n_deltas


def blob_sha(data):
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


# --------------------------------------------------------------------------- HTTP

class Resp:
    def __init__(self, status, headers, body, ms, fresh):
        self.status, self.headers, self.body, self.ms, self.fresh = status, headers, body, ms, fresh

    def json(self):
        return json.loads(self.body or b"null")


class Http:
    """One kept-alive connection. `fresh` on a response means it paid for a new TCP+TLS connection."""

    def __init__(self, base_url, headers=None, timeout=300):
        u = urllib.parse.urlsplit(base_url)
        self.https = u.scheme == "https"
        self.host, self.port = u.hostname, u.port
        self.prefix = u.path.rstrip("/")
        self.headers = {"User-Agent": USER_AGENT, **(headers or {})}
        self.timeout = timeout
        self.conn = None

    def close(self):
        if self.conn:
            self.conn.close()
        self.conn = None

    def request(self, method, path, body=None, headers=None):
        merged = {**self.headers, **(headers or {})}
        for attempt in (0, 1):
            fresh = self.conn is None
            t0 = time.perf_counter()
            if fresh:
                # Short per-address connect timeout: some CDN addresses are unreachable from some
                # networks, and the OS default (21 s on Windows) would stall the whole run.
                cls = http.client.HTTPSConnection if self.https else http.client.HTTPConnection
                self.conn = cls(self.host, self.port, timeout=CONNECT_TIMEOUT)
                self.conn.connect()
                self.conn.sock.settimeout(self.timeout)
            try:
                self.conn.request(method, self.prefix + path, body=body, headers=merged)
                r = self.conn.getresponse()
                data = r.read()
            except (http.client.RemoteDisconnected, http.client.CannotSendRequest,
                    ConnectionResetError, ConnectionAbortedError, BrokenPipeError):
                self.close()
                if attempt == 0 and not fresh:
                    continue  # the server closed an idle keep-alive connection; retry once
                raise
            ms = (time.perf_counter() - t0) * 1000
            if r.will_close:
                self.close()
            return Resp(r.status, {k.lower(): v for k, v in r.getheaders()}, data, ms, fresh)
        raise AssertionError("unreachable")


# --------------------------------------------------------------------------- targets

class Target:
    def __init__(self, git_url, token=None, repo=None, git_user="x-access-token", local=False):
        self.git_url = git_url.rstrip("/")
        self.token = token
        self.repo = repo
        self.local = local
        self.basic = "Basic " + base64.b64encode(f"{git_user}:{token}".encode()).decode() if token else None

    @classmethod
    def github(cls, repo, token, git_user="x-access-token"):
        return cls(f"https://github.com/{repo}.git", token, repo, git_user)

    def git_env(self):
        """Environment for git subprocesses. Auth goes in config env vars, never in argv or URLs."""
        env = dict(os.environ)
        env.update(GIT_TERMINAL_PROMPT="0", GCM_INTERACTIVE="never")
        cfg = [("credential.helper", ""), ("protocol.version", "2")]
        if self.basic:
            cfg.append(("http.extraHeader", f"Authorization: {self.basic}"))
        env["GIT_CONFIG_COUNT"] = str(len(cfg))
        for i, (k, v) in enumerate(cfg):
            env[f"GIT_CONFIG_KEY_{i}"], env[f"GIT_CONFIG_VALUE_{i}"] = k, v
        return env

    def git_http(self):
        return Http(self.git_url, {"Authorization": self.basic} if self.basic else {})

    def api_http(self, token=None):
        return Http("https://api.github.com", {
            "Authorization": f"Bearer {token or self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })


# --------------------------------------------------------------------------- git protocol (raw)

def receive_pack(conn, ref, old, new, pack):
    """One-request push: ref update command plus packfile. Returns (resp, ok, status_lines)."""
    command = f"{old or ZERO_SHA} {new} {ref}\0report-status agent=octospike/{VERSION}\n"
    r = conn.request("POST", "/git-receive-pack", pkt(command) + FLUSH + pack, {
        "Content-Type": "application/x-git-receive-pack-request",
        "Accept": "application/x-git-receive-pack-result",
    })
    lines = pkt_lines(r.body) if r.status == 200 else [f"HTTP {r.status}: {r.body[:200]!r}"]
    ok = r.status == 200 and "unpack ok" in lines and f"ok {ref}" in lines
    return r, ok, lines


def v2_request(conn, command, args):
    body = pkt(f"command={command}\n") + pkt(f"agent=octospike/{VERSION}\n") + DELIM
    body += b"".join(pkt(a + "\n") for a in args) + FLUSH
    return conn.request("POST", "/git-upload-pack", body, {
        "Content-Type": "application/x-git-upload-pack-request",
        "Accept": "application/x-git-upload-pack-result",
        "Git-Protocol": "version=2",
    })


def ls_refs(conn, prefix):
    r = v2_request(conn, "ls-refs", [f"ref-prefix {prefix}"])
    if r.status != 200:
        raise SpikeError(f"ls-refs failed: HTTP {r.status}")
    refs = {}
    for line in pkt_lines(r.body):
        sha, _, name = line.partition(" ")
        refs[name.split(" ")[0]] = sha
    return r, refs


def fetch_blobs(conn, shas):
    """Protocol v2 fetch of explicit object ids in one request. Returns (resp, pack_bytes, errors)."""
    args = ["no-progress", "ofs-delta"] + [f"want {s}" for s in shas] + ["done"]
    r = v2_request(conn, "fetch", args)
    pack, errors, in_pack = bytearray(), [], False
    if r.status != 200:
        return r, b"", [f"HTTP {r.status}: {r.body[:200]!r}"]
    for p in parse_pkts(r.body):
        if isinstance(p, int):
            continue
        if not in_pack:
            if p.rstrip(b"\n") == b"packfile":
                in_pack = True
            elif p.startswith(b"ERR"):
                errors.append(p.decode("utf-8", "replace").strip())
            continue
        if p[0] == 1:
            pack += p[1:]
        elif p[0] == 3:
            errors.append(p[1:].decode("utf-8", "replace").strip())
    return r, bytes(pack), errors


def advertisement(conn, service):
    """GET info/refs. Returns (resp, capabilities list). Also serves as the TLS warm-up."""
    headers = {"Git-Protocol": "version=2"} if service == "git-upload-pack" else {}
    r = conn.request("GET", f"/info/refs?service={service}", headers=headers)
    if r.status != 200:
        raise SpikeError(f"{service} advertisement failed: HTTP {r.status} {r.body[:200]!r}")
    lines = [p for p in parse_pkts(r.body) if isinstance(p, bytes)]
    caps = []
    if service == "git-upload-pack":
        caps = [line.decode().strip() for line in lines[1:] if not line.startswith(b"version")]
    elif len(lines) > 1 and b"\0" in lines[1]:
        caps = lines[1].split(b"\0", 1)[1].decode().split()
    return r, caps


# --------------------------------------------------------------------------- workbench

def page_path(pid):
    return f"pages/{(pid >> 16) & 255:02x}/{(pid >> 8) & 255:02x}/{pid & 255:02x}"


class Workbench:
    """A local bare repo mirroring one benchmark branch on the target."""

    def __init__(self, target, workdir, run_id):
        self.target = target
        self.env = target.git_env()
        self.dir = Path(workdir) / "local.git"
        self.run_id = run_id
        self.ref = f"refs/heads/octospike/{run_id}"
        self.stage_ref = f"refs/octopage/stage/{run_id}"
        self.head = None
        self.root = None
        self.pool = []  # page blob SHAs reachable from the benchmark branch
        self.counter = 0
        self.git("init", "--bare", "-q", str(self.dir), cwd=None)

    def git(self, *args, cwd="repo", input=None, check=True):
        p = subprocess.run(["git", *args], cwd=self.dir if cwd == "repo" else cwd, env=self.env,
                           input=input, capture_output=True)
        if check and p.returncode:
            raise SpikeError(f"git {args[0]} failed: {p.stderr.decode('utf-8', 'replace').strip()}")
        return p

    def make_commit(self, parent, n_pages):
        """Commit n random 4 KB pages plus a changelog blob on top of `parent`. Returns (sha, page blob SHAs)."""
        self.counter += 1
        tmp_ref = f"refs/octospike-tmp/{self.counter}"
        msg = f"octospike txn {self.counter}".encode()
        out = bytearray()
        out += f"commit {tmp_ref}\ncommitter octospike <octospike@invalid> {int(time.time())} +0000\n".encode()
        out += b"data %d\n%s\n" % (len(msg), msg)
        if parent:
            out += f"from {parent}\n".encode()
        blobs = []
        for pid in random.sample(range(1, DB_PAGES), n_pages):
            data = os.urandom(PAGE_SIZE)  # encrypted pages look exactly like this
            blobs.append(blob_sha(data))
            out += f"M 100644 inline {page_path(pid)}\ndata {PAGE_SIZE}\n".encode() + data + b"\n"
        changelog = os.urandom(300)
        out += f"M 100644 inline changelog\ndata {len(changelog)}\n".encode() + changelog + b"\n"
        out += b"done\n"
        self.git("fast-import", "--quiet", "--done", "--force", input=bytes(out))
        sha = self.git("rev-parse", tmp_ref).stdout.decode().strip()
        return sha, blobs

    def build_pack(self, new, old):
        """Thin pack of everything in `new` not in `old`, as `git push` would send."""
        revs = f"{new}\n" + (f"^{old}\n" if old else "")
        return self.git("pack-objects", "--stdout", "--revs", "--thin", "--delta-base-offset", "-q",
                        input=revs.encode()).stdout

    def push_cli(self, old, new, ref=None):
        ref = ref or self.ref
        t0 = time.perf_counter()
        p = self.git("push", "--porcelain", "--no-verify", f"--force-with-lease={ref}:{old or ''}",
                     self.target.git_url, f"{new}:{ref}", check=False)
        ms = (time.perf_counter() - t0) * 1000
        return ms, p.returncode == 0, (p.stdout + p.stderr).decode("utf-8", "replace").strip()

    def force_push_cli(self, new, ref):
        self.git("push", "--force", "--no-verify", "-q", self.target.git_url, f"{new}:{ref}")

    def stage(self, sha):
        """Put `sha` on the remote without moving the branch (spec: refs/octopage/stage/<txid>)."""
        try:
            self.force_push_cli(sha, self.stage_ref)
        except SpikeError:
            if not self.stage_ref.startswith("refs/octopage/"):
                raise
            log(f"  {self.stage_ref} refused; staging under refs/heads instead (see refs checks)")
            self.stage_ref = f"refs/heads/octospike/{self.run_id}-stage"
            self.force_push_cli(sha, self.stage_ref)

    def delete_remote_ref(self, ref):
        return self.git("push", "--no-verify", "-q", self.target.git_url, f":{ref}", check=False).returncode == 0

    def init_remote(self):
        sha, blobs = self.make_commit(None, 1)
        ms, ok, detail = self.push_cli(None, sha)
        if not ok:
            raise SpikeError(f"could not create {self.ref} on the target: {detail}")
        self.head = self.root = sha
        self.pool.extend(blobs)

    def resync(self, conn):
        _, refs = ls_refs(conn, self.ref)
        self.head = refs.get(self.ref, self.head)

    def ensure_pool(self, needed):
        """Top up the reachable blob pool with unmeasured pushes if the push suite did not run."""
        while len(self.pool) < needed:
            sha, blobs = self.make_commit(self.head, min(2000, needed - len(self.pool)))
            ms, ok, detail = self.push_cli(self.head, sha)
            if not ok:
                raise SpikeError(f"pool top-up push failed: {detail}")
            self.head = sha
            self.pool.extend(blobs)


# --------------------------------------------------------------------------- recording

def summarize(values):
    s = sorted(values)
    n = len(s)
    if not n:
        return None

    def q(p):
        k = (n - 1) * p
        f = int(k)
        c = min(f + 1, n - 1)
        return s[f] + (s[c] - s[f]) * (k - f)

    return {"n": n, "p50": round(q(0.5), 1), "p95": round(q(0.95), 1),
            "min": round(s[0], 1), "max": round(s[-1], 1), "mean": round(sum(s) / n, 1)}


class Recorder:
    def __init__(self, meta):
        self.meta = meta
        self.cases = {}
        self.checks = {}
        self.errors = {}
        self.info = {}

    def sample(self, case, ms, fresh=False, **extra):
        self.cases.setdefault(case, []).append({"ms": round(ms, 2), "fresh": fresh, **extra})
        tag = " (new connection)" if fresh else ""
        log(f"  {case:<28} {ms:9.1f} ms{tag}")

    def check(self, name, passed, detail=""):
        self.checks.setdefault(name, {"passed": True, "details": []})
        self.checks[name]["passed"] &= bool(passed)
        if detail:
            self.checks[name]["details"].append(detail)
        log(f"  check {name}: {'PASS' if passed else 'FAIL'} {detail}")

    def error(self, suite, exc):
        self.errors[suite] = "".join(traceback.format_exception_only(type(exc), exc)).strip()
        log(f"  ERROR in {suite}: {self.errors[suite]}")

    def to_json(self):
        cases = {}
        for name, samples in self.cases.items():
            warm = [s["ms"] for s in samples if not s["fresh"]]
            cases[name] = {"stats": summarize(warm or [s["ms"] for s in samples]),
                           "warm_only": bool(warm), "samples": samples}
        return {"meta": self.meta, "info": self.info, "cases": cases, "checks": self.checks, "errors": self.errors}


# --------------------------------------------------------------------------- suites

def suite_push(bench, rec, reps):
    conn = bench.target.git_http()
    _, caps = advertisement(conn, "git-receive-pack")
    rec.info["receive_pack_capabilities"] = caps
    for size, (n_raw, n_cli) in reps.items():
        log(f"[push] {size} page(s): {n_raw} raw, {n_cli} cli")
        for i in range(max(n_raw, n_cli)):
            for method, n in (("raw", n_raw), ("cli", n_cli)):
                if i >= n:
                    continue
                new, blobs = bench.make_commit(bench.head, size)
                if method == "raw":
                    t0 = time.perf_counter()
                    pack = bench.build_pack(new, bench.head)
                    build_ms = (time.perf_counter() - t0) * 1000
                    r, ok, lines = receive_pack(conn, bench.ref, bench.head, new, pack)
                    if not ok:
                        raise SpikeError(f"raw push rejected: {lines}")
                    rec.sample(f"push/raw/{size}", r.ms, r.fresh, pack_kb=round(len(pack) / 1024, 1),
                               build_ms=round(build_ms, 1))
                else:
                    ms, ok, detail = bench.push_cli(bench.head, new)
                    if not ok:
                        raise SpikeError(f"cli push rejected: {detail}")
                    rec.sample(f"push/cli/{size}", ms)
                bench.head = new
                bench.pool.extend(blobs)


def suite_ref_read(bench, rec, n):
    conn = bench.target.git_http()
    advertisement(conn, "git-upload-pack")
    log(f"[ref_read] {n} ls-refs" + ("" if bench.target.local else f", {n} REST"))
    for _ in range(n):
        r, refs = ls_refs(conn, bench.ref)
        rec.check("ref-read/ls-refs-returns-head", refs.get(bench.ref) == bench.head)
        rec.sample("ref-read/git-ls-refs", r.ms, r.fresh)
    if bench.target.local:
        return
    api = bench.target.api_http()
    path = f"/repos/{bench.target.repo}/git/ref/{bench.ref[len('refs/'):]}"
    for _ in range(n):
        r = api.request("GET", path)
        rec.check("ref-read/rest-returns-head", r.status == 200 and r.json()["object"]["sha"] == bench.head,
                  "" if r.status == 200 else f"HTTP {r.status}")
        rec.sample("ref-read/rest", r.ms, r.fresh)
    rec.info["rest_ratelimit_after_ref_reads"] = {k: v for k, v in r.headers.items() if k.startswith("x-ratelimit")}


def suite_fetch(bench, rec, reps, workdir):
    needed = sum(k * (a + b) for k, (a, b) in reps.items())
    bench.ensure_pool(needed)
    pool = bench.pool[:]
    random.shuffle(pool)

    def take(k):
        nonlocal pool
        batch, pool = pool[:k], pool[k:]
        return batch

    conn = bench.target.git_http()
    _, caps = advertisement(conn, "git-upload-pack")
    rec.info["upload_pack_v2_capabilities"] = caps
    for k, (n_raw, _) in reps.items():
        log(f"[fetch] raw protocol v2, {k} SHA(s) x {n_raw}")
        for _ in range(n_raw):
            shas = take(k)
            r, pack, errors = fetch_blobs(conn, shas)
            if errors or not pack:
                rec.check(f"fetch/raw-{k}-served", False, "; ".join(errors) or "empty response")
                continue
            count, objects, trailer_ok, n_deltas = parse_pack(pack)
            got = {blob_sha(data) for kind, data in objects if kind == 3}
            rec.check(f"fetch/raw-{k}-verified", trailer_ok and got == set(shas),
                      "" if got == set(shas) else f"asked {len(shas)}, got {count} objects, {len(got & set(shas))} match")
            rec.sample(f"fetch/raw/{k}", r.ms, r.fresh, pack_kb=round(len(pack) / 1024, 1), deltas=n_deltas)

    clone = Path(workdir) / f"partial-{bench.run_id}.git"
    branch = bench.ref[len("refs/heads/"):]
    bench.git("clone", "-q", "--bare", "--filter=blob:none", "--single-branch", "--branch", branch,
              bench.target.git_url, str(clone), cwd=None)

    def in_pack():
        out = bench.git("count-objects", "-v", cwd=clone).stdout.decode()
        return int(next(line.split(":")[1] for line in out.splitlines() if line.startswith("in-pack")))

    for k, (_, n_cli) in reps.items():
        if n_cli:
            log(f"[fetch] git cli lazy fetch, {k} SHA(s) x {n_cli}")
        for _ in range(n_cli):
            shas = take(k)
            before = in_pack()
            t0 = time.perf_counter()
            bench.git("-c", "fetch.negotiationAlgorithm=noop", "fetch", "-q", "--no-tags", "--no-write-fetch-head",
                      "--recurse-submodules=no", "--filter=blob:none", "--stdin", "origin",
                      cwd=clone, input=("\n".join(shas) + "\n").encode())
            ms = (time.perf_counter() - t0) * 1000
            rec.check(f"fetch/cli-{k}-verified", in_pack() - before == k)
            rec.sample(f"fetch/cli/{k}", ms)


def suite_raw_cdn(bench, rec, n):
    listing = bench.git("ls-tree", "-r", bench.head, "pages/").stdout.decode().splitlines()
    entries = random.sample(listing, min(n, len(listing)))
    raw = Http("https://raw.githubusercontent.com", {"Authorization": f"token {bench.target.token}"})
    raw.request("GET", f"/{bench.target.repo}/{bench.head}/octospike-warmup")  # TLS warm-up; 404 expected
    log(f"[raw_cdn] {len(entries)} pages by commit SHA, each read twice")
    for line in entries:
        meta, path = line.split("\t", 1)
        sha = meta.split()[2]
        url = f"/{bench.target.repo}/{bench.head}/{path}"
        for label in ("first", "repeat"):
            r = raw.request("GET", url)
            good = r.status == 200 and blob_sha(r.body) == sha
            rec.check("raw-cdn/serves-correct-bytes", good, "" if good else f"HTTP {r.status} for {path}")
            rec.sample(f"raw-cdn/{label}", r.ms, r.fresh, status=r.status, x_cache=r.headers.get("x-cache"))


def suite_rest(bench, rec, reps):
    api = bench.target.api_http()
    ref_path = f"/repos/{bench.target.repo}/git/refs/{bench.ref[len('refs/'):]}"
    before = api.request("GET", "/rate_limit").json()["resources"]["core"]
    log(f"[rest] {reps['ff']} fast-forward ref moves, {reps['reject']} stale ref moves")
    for _ in range(reps["ff"]):
        new, _ = bench.make_commit(bench.head, 1)
        bench.stage(new)  # objects must be on GitHub before the ref can point at them
        r = api.request("PATCH", ref_path, json.dumps({"sha": new, "force": False}),
                        {"Content-Type": "application/json"})
        rec.check("ref-move/rest-ff-accepted", r.status == 200, "" if r.status == 200 else f"HTTP {r.status} {r.body[:120]!r}")
        if r.status == 200:
            bench.head = new
        rec.sample("ref-move/rest-ff", r.ms, r.fresh)
    parent = bench.git("rev-parse", f"{bench.head}^").stdout.decode().strip()
    for _ in range(reps["reject"]):
        stale, _ = bench.make_commit(parent, 1)  # built on an old snapshot: not a fast-forward
        bench.stage(stale)
        r = api.request("PATCH", ref_path, json.dumps({"sha": stale, "force": False}),
                        {"Content-Type": "application/json"})
        rec.check("ref-move/rest-stale-rejected", r.status == 422, f"HTTP {r.status}")
        rec.sample("ref-move/rest-reject", r.ms, r.fresh)
    bench.delete_remote_ref(bench.stage_ref)
    after = api.request("GET", "/rate_limit").json()["resources"]["core"]
    rec.info["rest_core_limit"] = after["limit"]
    rec.info["rest_core_used_by_rest_suite"] = before["remaining"] - after["remaining"]


def suite_race(bench, rec, reps):
    c1, c2 = bench.target.git_http(), bench.target.git_http()
    for c in (c1, c2):
        advertisement(c, "git-receive-pack")
        advertisement(c, "git-upload-pack")
    log(f"[race] {reps['trials']} two-writer races, {reps['cas_exact']} exact-CAS probes, {reps['stale']} stale pushes")
    for _ in range(reps["trials"]):
        base = bench.head
        a, _ = bench.make_commit(base, 1)
        b, _ = bench.make_commit(base, 1)
        packs = {a: bench.build_pack(a, base), b: bench.build_pack(b, base)}
        barrier, results = threading.Barrier(2), {}

        def go(conn, new):
            barrier.wait()
            results[new] = receive_pack(conn, bench.ref, base, new, packs[new])

        threads = [threading.Thread(target=go, args=(c1, a)), threading.Thread(target=go, args=(c2, b))]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        winners = [sha for sha, (_, ok, _) in results.items() if ok]
        bench.resync(c1)
        rec.check("cas/race-exactly-one-winner", len(winners) == 1 and bench.head == winners[0],
                  f"winners={len(winners)}")
        for sha, (r, ok, lines) in results.items():
            rec.sample(f"cas/race-{'winner' if ok else 'loser'}", r.ms, r.fresh,
                       status=[line for line in lines if line.startswith(("ok", "ng"))])

    # A stale expected-old value whose new commit is still a fast-forward. A server that only
    # checks fast-forward accepts this; a true compare-and-swap rejects it.
    for _ in range(reps["cas_exact"]):
        new, _ = bench.make_commit(bench.head, 1)
        r, ok, lines = receive_pack(c1, bench.ref, bench.root, new, bench.build_pack(new, bench.head))
        rec.check("cas/server-verifies-expected-old", not ok, "; ".join(lines))
        bench.resync(c1)

    # A writer whose snapshot is stale: measures how long a lost race takes to report.
    for _ in range(reps["stale"]):
        base = bench.head
        winner, _ = bench.make_commit(base, 1)
        loser, _ = bench.make_commit(base, 1)
        _, ok, lines = receive_pack(c1, bench.ref, base, winner, bench.build_pack(winner, base))
        if not ok:
            raise SpikeError(f"setup push for stale test rejected: {lines}")
        bench.head = winner
        r, ok, lines = receive_pack(c2, bench.ref, base, loser, bench.build_pack(loser, base))
        rec.check("cas/stale-push-rejected", not ok, "; ".join(line for line in lines if line.startswith("ng")))
        rec.sample("cas/reject-stale", r.ms, r.fresh)
        bench.resync(c1)


def suite_refs(bench, rec):
    """Can OctoPage keep its own refs outside refs/heads (refs/octopage/...), as the spec assumes?"""
    conn = bench.target.git_http()
    api = None if bench.target.local else bench.target.api_http()
    log("[refs] custom ref namespace")
    for ref in (f"refs/octopage/stage/{bench.run_id}-refs", f"refs/octopage/lease-{bench.run_id}"):
        ms, ok, detail = bench.push_cli(None, bench.head, ref=ref)
        rec.check("refs/custom-namespace-push", ok, f"{ref}: {detail.splitlines()[-1] if detail else ''}")
        if not ok:
            continue
        _, refs = ls_refs(conn, ref)
        rec.check("refs/custom-namespace-listed", refs.get(ref) == bench.head, ref)
        if api:
            r = api.request("GET", f"/repos/{bench.target.repo}/git/ref/{ref[len('refs/'):]}")
            rec.check("refs/custom-namespace-rest-read", r.status == 200, f"{ref}: HTTP {r.status}")
        rec.check("refs/custom-namespace-delete", bench.delete_remote_ref(ref), ref)


def run_suites(bench, rec, reps, suites, workdir):
    bench.init_remote()
    for suite in suites:
        try:
            if suite == "push":
                suite_push(bench, rec, reps["push"])
            elif suite == "ref_read":
                suite_ref_read(bench, rec, reps["ref_read"])
            elif suite == "fetch":
                suite_fetch(bench, rec, reps["fetch"], workdir)
            elif suite == "raw_cdn" and reps["raw_cdn"] and not bench.target.local:
                suite_raw_cdn(bench, rec, reps["raw_cdn"])
            elif suite == "rest" and reps["rest"] and not bench.target.local:
                suite_rest(bench, rec, reps["rest"])
            elif suite == "race":
                suite_race(bench, rec, reps["race"])
            elif suite == "refs":
                suite_refs(bench, rec)
        except Exception as exc:  # keep going: one broken suite should not waste the whole run
            rec.error(suite, exc)
            try:
                bench.resync(bench.target.git_http())
            except Exception:
                pass


# --------------------------------------------------------------------------- commands

def scaled(reps, scale):
    def s(n):
        return max(1, round(n * scale)) if n else 0

    out = dict(reps)
    out["push"] = {k: (s(a), s(b)) for k, (a, b) in reps["push"].items()}
    out["fetch"] = {k: (s(a), s(b)) for k, (a, b) in reps["fetch"].items()}
    out["ref_read"] = s(reps["ref_read"])
    out["raw_cdn"] = s(reps["raw_cdn"])
    out["rest"] = {k: s(v) for k, v in reps["rest"].items()} if reps["rest"] else None
    out["race"] = {k: s(v) for k, v in reps["race"].items()}
    return out


def base_meta(region, target_desc, reps):
    git_version = subprocess.run(["git", "--version"], capture_output=True).stdout.decode().strip()
    return {
        "spike_version": VERSION,
        "region": region,
        "target": target_desc,
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "python": platform.python_version(),
        "git": git_version,
        "platform": platform.platform(),
        "reps": {k: ({str(kk): vv for kk, vv in v.items()} if isinstance(v, dict) else v) for k, v in reps.items()},
    }


def rmtree(path):
    def on_error(func, p, _exc):
        os.chmod(p, stat.S_IWRITE)  # git marks pack files read-only, which Windows refuses to delete
        func(p)

    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=on_error)
    else:
        shutil.rmtree(path, onerror=on_error)


def print_summary(result):
    log("\n=== summary ===")
    log(f"{'case':<28} {'n':>4} {'p50 ms':>10} {'p95 ms':>10}")
    for name, case in result["cases"].items():
        st = case["stats"]
        log(f"{name:<28} {st['n']:>4} {st['p50']:>10.1f} {st['p95']:>10.1f}")
    for name, chk in result["checks"].items():
        log(f"check {name:<40} {'PASS' if chk['passed'] else 'FAIL'}")
    for suite, err in result["errors"].items():
        log(f"error in {suite}: {err}")


def cmd_run(args):
    token = os.environ.get("OCTOSPIKE_TOKEN")
    if not token:
        sys.exit("Set OCTOSPIKE_TOKEN to a fine-grained token with Contents: read and write on the test repo.")
    region = args.region
    run_id = time.strftime("%Y%m%d-%H%M%S", time.gmtime()) + "-" + "".join(c if c.isalnum() else "-" for c in region)
    reps = scaled(GITHUB_REPS, args.scale)
    suites = args.suites.split(",") if args.suites else SUITES
    # Personal access tokens log in as the account owner; Actions and App tokens (ghs_) as x-access-token.
    git_user = args.git_user or ("x-access-token" if token.startswith("ghs_") else args.repo.split("/")[0])
    target = Target.github(args.repo, token, git_user)
    rec = Recorder(base_meta(region, f"github.com/{args.repo}", reps))
    try:
        rec.info["rate_limit_before"] = target.api_http().request("GET", "/rate_limit").json()["resources"]["core"]
    except Exception as exc:
        rec.error("rate_limit", exc)
    workdir = tempfile.mkdtemp(prefix="octospike-")
    bench = Workbench(target, workdir, run_id)
    try:
        run_suites(bench, rec, reps, suites, workdir)
    finally:
        if not args.keep and bench.head:
            bench.delete_remote_ref(bench.ref)
            bench.delete_remote_ref(bench.stage_ref)
        rmtree(workdir)
    rec.meta["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    result = rec.to_json()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"{run_id}.json"
    path.write_text(json.dumps(result, indent=1))
    print_summary(result)
    log(f"\nwrote {path}")
    return 1 if result["errors"] else 0


def cmd_selftest(args):
    """Run every suite that does not need github.com against a local git http-backend."""
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
    import local_git_remote

    workdir = tempfile.mkdtemp(prefix="octospike-selftest-")
    result = {"cases": {}, "checks": {}, "errors": {"selftest": "did not finish"}}
    try:
        local_git_remote.init_repo(Path(workdir) / "srv" / "bench.git")
        server = local_git_remote.serve(Path(workdir) / "srv", token="selftest-token")
        url = f"http://127.0.0.1:{server.server_port}/bench.git"
        target = Target(url, token="selftest-token", local=True)
        rec = Recorder(base_meta("local", url, SELFTEST_REPS))
        r = Target(url, local=True).git_http().request("GET", "/info/refs?service=git-upload-pack")
        rec.check("auth/unauthenticated-request-refused", r.status == 401, f"HTTP {r.status}")
        bench = Workbench(target, workdir, "selftest")
        run_suites(bench, rec, SELFTEST_REPS, SUITES, workdir)
        server.shutdown()
        result = rec.to_json()
        print_summary(result)
    finally:
        rmtree(workdir)
    failed = [n for n, c in result["checks"].items() if not c["passed"]]
    expected_cases = {"push/raw/1", "push/cli/1", "push/raw/5000", "ref-read/git-ls-refs", "fetch/raw/64",
                      "fetch/cli/512", "cas/race-winner", "cas/race-loser", "cas/reject-stale"}
    missing = expected_cases - set(result["cases"])
    if failed or result["errors"] or missing:
        log(f"\nSELFTEST FAILED: checks={failed} errors={list(result['errors'])} missing={sorted(missing)}")
        return 1
    log("\nSELFTEST PASSED")
    return 0


# --- GitHub App checks -------------------------------------------------------------

def make_app_jwt(app_id, key_path):
    def b64u(b):
        return base64.urlsafe_b64encode(b).rstrip(b"=")

    now = int(time.time())
    iss = int(app_id) if str(app_id).isdigit() else app_id
    header = b64u(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    payload = b64u(json.dumps({"iat": now - 60, "exp": now + 540, "iss": iss}).encode())
    signing_input = header + b"." + payload
    p = subprocess.run(["openssl", "dgst", "-sha256", "-sign", key_path], input=signing_input, capture_output=True)
    if p.returncode:
        raise SpikeError(f"openssl could not sign the JWT: {p.stderr.decode().strip()}")
    return (signing_input + b"." + b64u(p.stdout)).decode()


def device_flow_token(client_id):
    gh = Http("https://github.com", {"Accept": "application/json",
                                     "Content-Type": "application/x-www-form-urlencoded"})
    code = gh.request("POST", "/login/device/code", urllib.parse.urlencode({"client_id": client_id})).json()
    if "device_code" not in code:
        raise SpikeError(f"device flow not available: {code}")
    log(f"\nOpen {code['verification_uri']} and enter code {code['user_code']} to authorize the App as yourself.")
    deadline = time.time() + code.get("expires_in", 900)
    interval = code.get("interval", 5)
    while time.time() < deadline:
        time.sleep(interval)
        tok = gh.request("POST", "/login/oauth/access_token", urllib.parse.urlencode({
            "client_id": client_id, "device_code": code["device_code"],
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"})).json()
        if "access_token" in tok:
            return tok["access_token"]
        if tok.get("error") == "slow_down":
            interval += 5
        elif tok.get("error") != "authorization_pending":
            raise SpikeError(f"device flow failed: {tok}")
    raise SpikeError("device flow timed out")


def try_create_repo(rec, label, api, owner, owner_type, name):
    path = f"/orgs/{owner}/repos" if owner_type == "Organization" else "/user/repos"
    r = api.request("POST", path, json.dumps({"name": name, "private": True, "auto_init": False}),
                    {"Content-Type": "application/json"})
    message = r.json().get("message", "") if r.body else ""
    rec.check(f"app/{label}-can-create-repo", r.status == 201, f"POST {path}: HTTP {r.status} {message}")
    if r.status == 201:
        d = api.request("DELETE", f"/repos/{owner}/{name}")
        rec.check(f"app/{label}-can-delete-repo", d.status == 204, f"HTTP {d.status}")
        if d.status != 204:
            log(f"  NOTE: delete github.com/{owner}/{name} by hand")


def cmd_app(args):
    rec = Recorder(base_meta(args.region, f"github.com/{args.repo}", {}))
    rec.meta["kind"] = "github-app"
    jwt = make_app_jwt(args.app_id, args.key)
    api = Target.github(args.repo, "unused").api_http(token=jwt)
    r = api.request("POST", f"/app/installations/{args.installation}/access_tokens")
    rec.check("app/installation-token-minted", r.status == 201, f"HTTP {r.status}")
    if r.status != 201:
        print_summary(rec.to_json())
        return 1
    grant = r.json()
    rec.info["installation_token_expires_at"] = grant.get("expires_at")
    rec.info["installation_permissions"] = grant.get("permissions")
    target = Target.github(args.repo, grant["token"])
    inst_api = target.api_http()
    rec.info["installation_rate_limit"] = inst_api.request("GET", "/rate_limit").json()["resources"]["core"]

    workdir = tempfile.mkdtemp(prefix="octospike-app-")
    try:
        bench = Workbench(target, workdir, f"app-{int(time.time())}")
        conn = target.git_http()
        try:
            advertisement(conn, "git-upload-pack")
            rec.check("app/git-read", True)
            sha, _ = bench.make_commit(None, 1)
            r, ok, lines = receive_pack(conn, bench.ref, None, sha, bench.build_pack(sha, None))
            rec.check("app/git-push-with-installation-token", ok, "; ".join(lines))
            if ok:
                rec.check("app/git-delete-ref", bench.delete_remote_ref(bench.ref))
        except Exception as exc:
            rec.error("app-git", exc)
    finally:
        rmtree(workdir)

    if args.try_create_repo:
        owner = args.repo.split("/")[0]
        repo_info = inst_api.request("GET", f"/repos/{args.repo}").json()
        owner_type = repo_info.get("owner", {}).get("type", "User")
        rec.info["owner_type"] = owner_type
        try_create_repo(rec, "installation-token", inst_api, owner, owner_type, args.try_create_repo)
        if args.client_id:
            user_token = device_flow_token(args.client_id)
            try_create_repo(rec, "user-token", target.api_http(token=user_token), owner, owner_type,
                            args.try_create_repo)

    result = rec.to_json()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"app-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}.json"
    path.write_text(json.dumps(result, indent=1))
    print_summary(result)
    log(f"\nwrote {path}")
    return 0


# --- report ----------------------------------------------------------------------------

def spec_target(case):
    kind, _, rest = case.partition("/")
    if kind == "push":
        return SPEC_TARGETS.get(f"push/{rest.split('/')[-1]}", "")
    if kind == "fetch":
        return SPEC_TARGETS["fetch"]
    if case.startswith("ref-move/rest"):
        return SPEC_TARGETS["ref-move/rest"]
    return SPEC_TARGETS.get(case, "")


def cmd_report(args):
    runs = [json.loads(Path(f).read_text()) for f in args.files]
    runs = [r for r in runs if r["meta"].get("kind") != "github-app"]
    if not runs:
        sys.exit("no run results given")
    labels = [f"{r['meta']['region']} ({r['meta']['started_utc'][:10]})" for r in runs]
    cases = []
    for r in runs:
        cases += [c for c in r["cases"] if c not in cases]
    out = ["## Latency (ms, p50 / p95, n)", "",
           "| Case | Spec target | " + " | ".join(labels) + " |",
           "|---|---|" + "---|" * len(runs)]
    for case in cases:
        cells = []
        for r in runs:
            st = r["cases"].get(case, {}).get("stats")
            cells.append(f"{st['p50']:.0f} / {st['p95']:.0f} (n={st['n']})" if st else "")
        out.append(f"| `{case}` | {spec_target(case)} | " + " | ".join(cells) + " |")
    out += ["", "## Pack sizes (KB, median)", "", "| Case | " + " | ".join(labels) + " |", "|---|" + "---|" * len(runs)]
    for case in cases:
        cells = []
        for r in runs:
            kbs = sorted(s["pack_kb"] for s in r["cases"].get(case, {}).get("samples", []) if "pack_kb" in s)
            cells.append(f"{kbs[len(kbs) // 2]:.1f}" if kbs else "")
        if any(cells):
            out.append(f"| `{case}` | " + " | ".join(cells) + " |")
    checks = []
    for r in runs:
        checks += [c for c in r["checks"] if c not in checks]
    out += ["", "## Checks", "", "| Check | " + " | ".join(labels) + " |", "|---|" + "---|" * len(runs)]
    for c in checks:
        out.append(f"| `{c}` | " + " | ".join(
            ("PASS" if r["checks"][c]["passed"] else "FAIL") if c in r["checks"] else "" for r in runs) + " |")
    for label, r in zip(labels, runs):
        if r["errors"]:
            out += ["", f"**Errors in {label}:**"] + [f"- `{s}`: {e}" for s, e in r["errors"].items()]
    print("\n".join(out))
    return 0


def main():
    parser = argparse.ArgumentParser(description="OctoPage Phase 0 transport spike")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("selftest", help="run against a local git http-backend (no GitHub needed)")
    p.set_defaults(fn=cmd_selftest)

    p = sub.add_parser("run", help="measure a GitHub repo (token in OCTOSPIKE_TOKEN)")
    p.add_argument("--repo", required=True, help="OWNER/NAME of a throwaway test repo")
    p.add_argument("--region", required=True, help="label for where this runs, e.g. home or github-actions")
    p.add_argument("--suites", help="comma-separated subset of: " + ",".join(SUITES))
    p.add_argument("--scale", type=float, default=1.0, help="multiply sample counts (0.25 for a quick run)")
    p.add_argument("--out", default=str(Path(__file__).parent / "results"))
    p.add_argument("--keep", action="store_true", help="keep the benchmark branch on GitHub afterwards")
    p.add_argument("--git-user", help="HTTPS username for git (default: repo owner, or x-access-token for ghs_ tokens)")
    p.set_defaults(fn=cmd_run)

    p = sub.add_parser("app", help="check what a GitHub App installation token can do")
    p.add_argument("--repo", required=True)
    p.add_argument("--app-id", required=True, help="App ID or client ID")
    p.add_argument("--key", required=True, help="path to the App's private key (.pem)")
    p.add_argument("--installation", required=True, help="installation ID")
    p.add_argument("--try-create-repo", metavar="NAME", help="also test repo creation (creates, then deletes NAME)")
    p.add_argument("--client-id", help="App client ID, to also test a user token via device flow")
    p.add_argument("--region", default="app-check")
    p.add_argument("--out", default=str(Path(__file__).parent / "results"))
    p.set_defaults(fn=cmd_app)

    p = sub.add_parser("report", help="merge result files into Markdown tables")
    p.add_argument("files", nargs="+")
    p.set_defaults(fn=cmd_report)

    args = parser.parse_args()
    sys.exit(args.fn(args))


if __name__ == "__main__":
    main()
