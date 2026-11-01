"""End-to-end checks for OPS-02 against a running CraftCloud behind a reverse proxy.

Standard library only; run with `python3 -I ops02_e2e.py ...`. Used by the
installation tests in deploy/install/tests/integration (nginx and Apache
behind a self-signed certificate). Prints one line per check and exits 1 on
the first failure. It prints no secret: the bootstrap token is read from a
file and only ever sent to the server over TLS.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
import socket
import ssl
import sys
import time
import urllib.parse


def fail(message):
    print(f"not ok - {message}", flush=True)
    sys.exit(1)


def ok(message):
    print(f"ok - {message}", flush=True)


class Client:
    def __init__(self, base, source_address=None, extra_headers=None, public_url=None):
        parts = urllib.parse.urlsplit(base)
        self.host = parts.hostname
        self.port = parts.port or 443
        self.origin = public_url or f"{parts.scheme}://{parts.netloc}"
        self.context = ssl._create_unverified_context()  # self-signed test certificate
        self.source_address = (source_address, 0) if source_address else None
        self.extra_headers = extra_headers or {}
        self.token = None
        self.last_headers = []

    def request(self, method, path, body=None, headers=None, timeout=300):
        connection = http.client.HTTPSConnection(
            self.host, self.port, context=self.context, timeout=timeout, source_address=self.source_address
        )
        merged = {"Origin": self.origin}
        merged.update(self.extra_headers)
        if self.token:
            merged["Authorization"] = f"Bearer {self.token}"
        merged.update(headers or {})
        try:
            connection.request(method, path, body=body, headers=merged)
            response = connection.getresponse()
            data = response.read()
            self.last_headers = response.getheaders()
            return response.status, dict(response.getheaders()), data
        finally:
            connection.close()

    def json(self, method, path, payload=None, expect=(200, 201)):
        body = json.dumps(payload).encode() if payload is not None else None
        headers = {"Content-Type": "application/json"} if body is not None else {}
        status, _, data = self.request(method, path, body, headers)
        if status not in expect:
            fail(f"{method} {path} answered {status}: {data[:200]!r}")
        return json.loads(data) if data else {}


class Pattern:
    """A file-like object that streams a deterministic byte pattern of a given size."""

    def __init__(self, size):
        self.size = size
        self.sent = 0
        self.block = hashlib.sha256(b"ops02").digest() * 32768  # 1 MiB

    def digest(self):
        digest = hashlib.sha256()
        left = self.size
        while left:
            chunk = self.block[: min(len(self.block), left)]
            digest.update(chunk)
            left -= len(chunk)
        return digest.hexdigest()

    def read(self, amount=-1):
        if self.sent >= self.size:
            return b""
        amount = len(self.block) if amount is None or amount < 0 else amount
        amount = min(amount, len(self.block), self.size - self.sent)
        self.sent += amount
        return self.block[:amount]


def run_checks(args):
    client = Client(args.base, public_url=args.public_url or None)
    status, headers, data = client.request("GET", "/admin/")
    if status != 200 or b"<" not in data[:200]:
        fail(f"login page answered {status}")
    ok("the admin login page is served through the proxy")
    for path in ("/admin/", "/api/v1/info"):
        client.request("GET", path)
        names = [name.lower() for name, _ in client.last_headers]
        for header in ("x-content-type-options", "referrer-policy"):
            if names.count(header) != 1:
                fail(f"{path}: header {header} appears {names.count(header)} times, expected once (parity with the bundled Caddy)")
    ok("X-Content-Type-Options and Referrer-Policy appear exactly once, as with the bundled Caddy")
    status, _, data = client.request("GET", "/health/ready")
    if status != 200:
        fail(f"/health/ready answered {status}")
    ok("/health/ready through the proxy")
    status, _, _ = client.request("GET", "/metrics")
    if status != 404:
        fail(f"/metrics answered {status}, expected 404")
    ok("/metrics is not published")
    if args.hsts:
        if not any(key.lower() == "strict-transport-security" for key in headers):
            fail("HSTS header missing")
        ok("HSTS header present")

    with open(args.bootstrap_file, encoding="ascii") as handle:
        token = handle.read().strip()
    password = base64.urlsafe_b64encode(os.urandom(18)).decode()
    email = "operator@example.test"
    setup = client.json(
        "POST",
        "/api/v1/setup/complete",
        {
            "bootstrap_token": token,
            "email": email,
            "display_name": "Operator",
            "password": password,
            "organization": {"name": "Test", "slug": "test"},
        },
    )
    org = setup["organization_id"]
    login = client.json(
        "POST",
        "/api/v1/auth/device/login",
        {"email": email, "password": password, "client_kind": "cli", "client_id": "ops02-test", "device_name": "ops02"},
    )
    client.token = login["access_token"]
    workspace = client.json("POST", f"/api/v1/organizations/{org}/workspaces", {"name": "W", "slug": "w"})
    project = client.json("POST", f"/api/v1/workspaces/{workspace['id']}/projects", {"name": "P"})
    file = client.json("POST", f"/api/v1/projects/{project['id']}/files", {"name": "big.bin", "app": "other", "format": "bin"})
    ok("setup, login, workspace, project and file through the proxy")

    size = args.upload_mib * 1024 * 1024
    pattern = Pattern(size)
    upload = client.json("POST", f"/api/v1/files/{file['id']}/uploads", {"size_bytes": size, "sha256": pattern.digest()})
    started = time.time()
    status, _, data = client.request(
        "PUT",
        f"/api/v1/uploads/{upload['upload_id']}/content",
        pattern,
        {"Content-Length": str(size), "Content-Type": "application/octet-stream"},
        timeout=900,
    )
    if status not in (200, 201, 204):
        fail(f"upload of {args.upload_mib} MiB answered {status}: {data[:200]!r}")
    state = client.json("GET", f"/api/v1/uploads/{upload['upload_id']}")
    if state.get("state") not in ("stored", "committed"):
        fail(f"upload state is {state.get('state')}")
    ok(f"upload of {args.upload_mib} MiB without interruption ({time.time() - started:.1f} s), state {state['state']}")

    spoof_checks(args)


def spoof_checks(args):
    """X-Forwarded-For sent by the outside must not be believed.

    Failed logins are limited per account and client address (five in 15
    minutes). If the proxy let a forged header through, every attempt would
    look like a new address and the limit would never trigger.
    """
    victim = "victim@example.test"
    spoofed = Client(args.base, extra_headers={}, public_url=args.public_url or None)
    statuses = []
    for number in range(9):
        spoofed.extra_headers = {"X-Forwarded-For": f"203.0.113.{number + 10}", "X-Forwarded-Host": "evil.example", "Forwarded": "for=198.51.100.9"}
        status, _, _ = spoofed.request(
            "POST", "/api/v1/auth/login", json.dumps({"email": victim, "password": "wrong password 123"}).encode(),
            {"Content-Type": "application/json"},
        )
        statuses.append(status)
    if 429 not in statuses:
        fail(f"forged X-Forwarded-For values avoided the rate limit: {statuses}")
    ok(f"forged X-Forwarded-For does not avoid the failed-login limit {statuses}")
    if args.second_address:
        parts = urllib.parse.urlsplit(args.base)
        other_base = f"{parts.scheme}://{args.second_address}:{parts.port or 443}"
        other = Client(other_base, source_address=args.second_address, public_url=args.public_url or None)
        status, _, _ = other.request(
            "POST", "/api/v1/auth/login", json.dumps({"email": victim, "password": "wrong password 123"}).encode(),
            {"Content-Type": "application/json"},
        )
        if status != 401:
            fail(f"a different real client address shares the limit (status {status}); the proxy address is used instead of the client address")
        ok("a different real client address has its own limit (the proxy passes the real address on)")


def websocket_check(args):
    """The FigCraft path must answer a WebSocket handshake like the FigCraft server itself."""

    def handshake(host, port, tls, path):
        raw = socket.create_connection((host, port), timeout=15)
        conn = ssl._create_unverified_context().wrap_socket(raw, server_hostname=host) if tls else raw
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        conn.sendall(request.encode())
        conn.settimeout(15)
        head = b""
        while b"\r\n\r\n" not in head and len(head) < 8192:
            chunk = conn.recv(4096)
            if not chunk:
                break
            head += chunk
        conn.close()
        line = head.split(b"\r\n", 1)[0].decode("latin1")
        return line

    direct = handshake("127.0.0.1", args.figcraft_port, False, "/figcraft/")
    if not args.base:
        print(f"direct: {direct}", flush=True)
        if " 101 " not in direct + " ":
            fail("the FigCraft server did not accept a plain WebSocket handshake")
        ok("the FigCraft server accepts a WebSocket handshake (101)")
        return
    parts = urllib.parse.urlsplit(args.base)
    proxied = handshake(parts.hostname, parts.port or 443, True, "/figcraft/")
    print(f"direct: {direct}\nproxied: {proxied}", flush=True)
    if direct.split(" ")[1:2] != proxied.split(" ")[1:2]:
        fail("the proxy answers the FigCraft WebSocket handshake differently from the FigCraft server")
    if " 101 " not in direct + " ":
        fail("the FigCraft server did not accept a plain WebSocket handshake; cannot show the upgrade through the proxy")
    ok("WebSocket handshake on /figcraft/ is upgraded through the proxy (101 like the FigCraft server)")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="", help="https://127.0.0.1:18443")
    parser.add_argument("--public-url", default="", help="the configured public URL, sent as Origin")
    parser.add_argument("--only-spoof", action="store_true")
    parser.add_argument("--direct-websocket", type=int, default=0, help="only check this FigCraft port directly")
    parser.add_argument("--bootstrap-file")
    parser.add_argument("--upload-mib", type=int, default=100)
    parser.add_argument("--figcraft-port", type=int, default=0)
    parser.add_argument("--second-address", default="")
    parser.add_argument("--hsts", action="store_true")
    parser.add_argument("--only-websocket", action="store_true")
    parser.add_argument("--only-login", action="store_true")
    args = parser.parse_args()
    if args.direct_websocket:
        args.figcraft_port = args.direct_websocket
        args.base = ""
        websocket_check(args)
        return
    if args.only_spoof:
        spoof_checks(args)
        return
    if args.only_websocket:
        websocket_check(args)
        return
    if args.only_login:
        client = Client(args.base, public_url=args.public_url or None)
        status, _, _ = client.request("GET", "/admin/")
        if status != 200:
            fail(f"login page answered {status}")
        ok("the admin login page is served through the proxy")
        return
    run_checks(args)
    if args.figcraft_port:
        websocket_check(args)


if __name__ == "__main__":
    main()
