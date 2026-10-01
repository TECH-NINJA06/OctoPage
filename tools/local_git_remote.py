import argparse
import base64
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# Config that makes a local bare repo behave like a GitHub repo for OctoPage's purposes.
REPO_CONFIG = {
    "http.receivepack": "true",
    "uploadpack.allowFilter": "true",
    "uploadpack.allowAnySHA1InWant": "true",
    "receive.denyDeletes": "false",
}


def init_repo(path):
    """Create a bare repository at `path` configured for smart-HTTP push and partial clone."""
    path = Path(path)
    subprocess.run(["git", "init", "--bare", "-q", str(path)], check=True)
    for key, value in REPO_CONFIG.items():
        subprocess.run(["git", "config", "--file", str(path / "config"), key, value], check=True)
    return path


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if self.server.verbose:
            sys.stderr.write("[local-remote] " + (fmt % args) + "\n")

    def do_GET(self):
        self._backend()

    def do_POST(self):
        self._backend()

    def _authorized(self):
        token = self.server.token
        if not token:
            return True
        header = self.headers.get("Authorization", "")
        if not header.startswith("Basic "):
            return False
        try:
            _, _, password = base64.b64decode(header[6:]).decode().partition(":")
        except ValueError:
            return False
        return password == token

    def _read_body(self):
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            chunks = []
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return b"".join(chunks)
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _send(self, status, headers, payload):
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _backend(self):
        body = self._read_body()
        if not self._authorized():
            self._send(401, [("WWW-Authenticate", 'Basic realm="octopage-local"')], b"")
            return
        path, _, query = self.path.partition("?")
        env = dict(os.environ)
        env.update(
            GIT_PROJECT_ROOT=self.server.root,
            GIT_HTTP_EXPORT_ALL="1",
            PATH_INFO=path,
            QUERY_STRING=query,
            REQUEST_METHOD=self.command,
            CONTENT_TYPE=self.headers.get("Content-Type", ""),
            CONTENT_LENGTH=str(len(body)),
            REMOTE_USER="octopage",
            REMOTE_ADDR=self.client_address[0],
            SERVER_PROTOCOL="HTTP/1.1",
        )
        if self.headers.get("Git-Protocol"):
            env["HTTP_GIT_PROTOCOL"] = self.headers["Git-Protocol"]
        if self.headers.get("Content-Encoding"):
            env["HTTP_CONTENT_ENCODING"] = self.headers["Content-Encoding"]
        if path.endswith("/git-receive-pack"):
            # One push at a time: concurrent receive-packs carrying the same object fail on
            # Windows ("unable to migrate objects to permanent storage"). GitHub has no such
            # limit, and a stale push still loses its compare-and-swap.
            with self.server.push_lock:
                proc = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True)
        else:
            proc = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True)
        head, sep, payload = proc.stdout.partition(b"\r\n\r\n")
        if not sep:
            head, sep, payload = proc.stdout.partition(b"\n\n")
        status, headers = 200, []
        for line in head.decode("latin-1").splitlines():
            key, _, value = line.partition(":")
            if key.lower() == "status":
                status = int(value.split()[0])
            elif key and key.lower() not in ("content-length", "transfer-encoding"):
                headers.append((key, value.strip()))
        if proc.returncode and not proc.stdout:
            status, payload = 500, proc.stderr
        self._send(status, headers, payload)


class _Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # Clients drop keep-alive connections at will (git does after a 401); that is not an error.
        if isinstance(sys.exc_info()[1], (ConnectionError, TimeoutError)):
            return
        super().handle_error(request, client_address)


def serve(root, host="127.0.0.1", port=0, token=None, verbose=False):
    """Start serving `root` in a background thread. Returns the server; `.server_port` is the port."""
    server = _Server((host, port), _Handler)
    server.daemon_threads = True
    server.root = str(Path(root).resolve())
    server.token = token
    server.verbose = verbose
    server.push_lock = threading.Lock()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("root", help="directory containing bare repositories")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--token", help="require Basic auth with this password")
    parser.add_argument("--init", metavar="NAME", help="create ROOT/NAME as a bare repo first")
    args = parser.parse_args()
    if args.init:
        init_repo(Path(args.root) / args.init)
    server = serve(args.root, args.host, args.port, args.token, verbose=True)
    print(f"serving {server.root} at http://{args.host}:{server.server_port}/ (Ctrl+C to stop)")
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        server.shutdown()


if __name__ == "__main__":
    main()
