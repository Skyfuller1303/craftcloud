"""Run isolated HTTPS Graph OAuth protocol tests with a unique local CA."""

import http.server
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import ssl
import urllib.parse
import shutil
import uuid
import base64
import hashlib
import json
import time


CASES = ("success", "expiry", "401", "429", "403", "404", "unknown", "redirect", "oversized", "untrusted_ca", "certificate_success", "certificate_mismatch", "certificate_expired", "interrupted", "token_429", "token_503")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def reply(self, status, body=b"", headers=()):
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request_body = self.rfile.read(length)
        case = os.environ["CRAFTCLOUD_OAUTH_TEST_CASE"]
        if self.path == "/token":
            self.server.token_count += 1
            present = b"client_secret=synthetic-secret-canary" in request_body
            self.server.secret_present = present if self.server.token_count == 1 else self.server.secret_present and present
            form = urllib.parse.parse_qs(request_body.decode("ascii"), keep_blank_values=True)
            if case.startswith("certificate_"):
                valid = validate_client_assertion(form, pathlib.Path(os.environ["CRAFTCLOUD_OAUTH_TEST_CERT"]), self.server.seen_jti)
                self.server.certificate_assertion_valid = valid
                self.server.credential_present = valid
                if not valid:
                    return self.reply(401, b'{"error":"invalid_client"}')
            else:
                self.server.credential_present = self.server.secret_present
            valid_scope = form.get("scope") == ["https://graph.microsoft.com/.default"]
            self.server.graph_scope_valid = valid_scope if self.server.token_count == 1 else self.server.graph_scope_valid and valid_scope
            if case == "redirect":
                return self.reply(302, headers=(("Location", "/redirected"),))
            if case == "oversized":
                return self.reply(200, b'{"padding":"' + b"x" * 65540 + b'"}')
            if case == "unknown":
                return self.reply(400, b'{"error":"synthetic-secret-canary"}')
            if case in ("token_429", "token_503"):
                return self.reply(int(case[6:]), b'{"error":"temporary_issue"}', (("Retry-After", "17"),))
            if case == "interrupted":
                self.send_response(200)
                self.send_header("Content-Length", "128")
                self.end_headers()
                self.wfile.write(b"{}")
                self.wfile.flush()
                self.close_connection = True
                return
            access = f"test-token-{self.server.token_count}".encode()
            expiry = 1 if case == "expiry" else 3600
            return self.reply(200, b'{"token_type":"Bearer","expires_in":' + str(expiry).encode() + b',"access_token":"' + access + b'"}', (("Content-Type", "application/json"),))
        self.server.graph_count += 1
        first_graph = self.server.graph_count == 1
        auth = self.headers.get("Authorization", "").startswith("Bearer ")
        sender = "/sender@example.invalid/sendMail" in self.path
        try:
            import json
            payload = json.loads(request_body or b"{}")
        except Exception:
            payload = {}
        save_false = payload.get("saveToSentItems") is False
        self.server.authorization_present = auth if first_graph else self.server.authorization_present and auth
        self.server.sender_path_valid = sender if first_graph else self.server.sender_path_valid and sender
        self.server.save_to_sent_items_false = save_false if first_graph else self.server.save_to_sent_items_false and save_false
        if case == "401" and self.server.graph_count == 1:
            return self.reply(401, b"{}")
        statuses = {"429": (429, b"{}", (("Retry-After", "12"),)), "403": (403, b'{"error":{"code":"ErrorAccessDenied"}}', ()), "404": (404, b'{"error":{"code":"ErrorRecipientNotFound"}}', ())}
        if case in statuses:
            return self.reply(*statuses[case])
        return self.reply(202)


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def generate_with_host_openssl(root):
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "ca.key"), "-out", str(root / "ca.pem"), "-days", "1", "-subj", "/CN=CraftCloud Test CA")
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "wrong-ca.key"), "-out", str(root / "wrong-ca.pem"), "-days", "1", "-subj", "/CN=Untrusted CraftCloud Test CA")
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "server.key"), "-out", str(root / "server.csr"), "-subj", "/CN=127.0.0.1")
    (root / "extensions.cnf").write_text("subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n", encoding="ascii")
    run("openssl", "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / "server.pem"), "-days", "1", "-extfile", str(root / "extensions.cnf"))
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "client.key"), "-out", str(root / "client.csr"), "-subj", "/CN=synthetic-client")
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "wrong-client.key"), "-out", str(root / "wrong-client.csr"), "-subj", "/CN=synthetic-client")
    run("openssl", "x509", "-req", "-in", str(root / "client.csr"), "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / "client.pem"), "-days", "1")
    run("openssl", "x509", "-req", "-in", str(root / "client.csr"), "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-out", str(root / "expired-client.pem"), "-days", "0")
    for name in ("ca.key", "wrong-ca.key", "server.key", "client.key", "wrong-client.key"):
        (root / name).chmod(0o600)


def b64url_decode(value):
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def validate_client_assertion(form, certificate, server_jtis):
    assertions = form.get("client_assertion", [])
    if len(assertions) != 1:
        return False
    assertion = assertions[0]
    try:
        encoded_header, encoded_claims, encoded_signature = assertion.split(".")
        header = json.loads(b64url_decode(encoded_header))
        claims = json.loads(b64url_decode(encoded_claims))
        der_path = certificate.with_suffix(".der")
        public_path = certificate.with_suffix(".pub.pem")
        signature_path = certificate.with_suffix(".sig")
        message_path = certificate.with_suffix(".message")
        run("openssl", "x509", "-in", str(certificate), "-outform", "DER", "-out", str(der_path))
        thumbprint = base64.urlsafe_b64encode(hashlib.sha256(der_path.read_bytes()).digest()).decode().rstrip("=")
        run("openssl", "x509", "-in", str(certificate), "-pubkey", "-noout", "-out", str(public_path))
        signature_path.write_bytes(b64url_decode(encoded_signature))
        message_path.write_text(encoded_header + "." + encoded_claims, encoding="ascii")
        verified = subprocess.run(("openssl", "pkeyutl", "-verify", "-pubin", "-inkey", str(public_path), "-rawin", "-digest", "sha256", "-in", str(message_path), "-sigfile", str(signature_path), "-pkeyopt", "rsa_padding_mode:pss", "-pkeyopt", "rsa_pss_saltlen:digest"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False).returncode == 0
        now = int(time.time())
        expected_aud = "https://login.microsoftonline.com/12345678-1234-1234-1234-123456789012/oauth2/v2.0/token"
        jti = claims.get("jti")
        try:
            parsed_jti = uuid.UUID(jti)
        except (ValueError, TypeError, AttributeError):
            return False
        unique_jti = parsed_jti not in server_jtis
        server_jtis.add(parsed_jti)
        return header.get("alg") == "PS256" and header.get("x5t#S256") == thumbprint and claims.get("iss") == "synthetic-client" and claims.get("sub") == "synthetic-client" and claims.get("aud") == expected_aud and unique_jti and isinstance(claims.get("exp"), int) and isinstance(claims.get("iat"), int) and 0 < claims["exp"] - claims["iat"] <= 300 and claims["iat"] <= now <= claims["exp"] and verified
    except (ValueError, OSError, KeyError, json.JSONDecodeError):
        return False


def generate_with_docker(root):
    tag = uuid.uuid4().hex[:12]
    image = f"craftcloud-oauth-certs:{tag}"
    container = f"craftcloud-oauth-certs-{tag}"
    volume = f"craftcloud-oauth-certs-{tag}"
    dockerfile = root / "Dockerfile"
    dockerfile.write_text("FROM python:3.12-alpine\nRUN apk add --no-cache openssl\nCMD [\"tail\", \"-f\", \"/dev/null\"]\n", encoding="ascii")
    try:
        run("docker", "volume", "create", volume)
        run("docker", "build", "--file", str(dockerfile), "--tag", image, str(root))
        run("docker", "run", "--detach", "--name", container, "--volume", f"{volume}:/certs", image)
        run("docker", "exec", container, "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "/certs/ca.key", "-out", "/certs/ca.pem", "-days", "1", "-subj", "/CN=CraftCloud Test CA")
        run("docker", "exec", container, "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "/certs/wrong-ca.key", "-out", "/certs/wrong-ca.pem", "-days", "1", "-subj", "/CN=Untrusted CraftCloud Test CA")
        run("docker", "exec", container, "openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "/certs/server.key", "-out", "/certs/server.csr", "-subj", "/CN=127.0.0.1")
        run("docker", "exec", container, "sh", "-c", "printf 'subjectAltName=IP:127.0.0.1\\nextendedKeyUsage=serverAuth\\n' > /certs/extensions.cnf")
        run("docker", "exec", container, "openssl", "x509", "-req", "-in", "/certs/server.csr", "-CA", "/certs/ca.pem", "-CAkey", "/certs/ca.key", "-CAcreateserial", "-out", "/certs/server.pem", "-days", "1", "-extfile", "/certs/extensions.cnf")
        run("docker", "exec", container, "openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "/certs/client.key", "-out", "/certs/client.csr", "-subj", "/CN=synthetic-client")
        run("docker", "exec", container, "openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "/certs/wrong-client.key", "-out", "/certs/wrong-client.csr", "-subj", "/CN=synthetic-client")
        run("docker", "exec", container, "openssl", "x509", "-req", "-in", "/certs/client.csr", "-CA", "/certs/ca.pem", "-CAkey", "/certs/ca.key", "-CAcreateserial", "-out", "/certs/client.pem", "-days", "1")
        run("docker", "exec", container, "openssl", "x509", "-req", "-in", "/certs/client.csr", "-CA", "/certs/ca.pem", "-CAkey", "/certs/ca.key", "-CAcreateserial", "-out", "/certs/expired-client.pem", "-days", "0")
        run("docker", "exec", container, "chmod", "600", "/certs/ca.key", "/certs/wrong-ca.key", "/certs/server.key", "/certs/client.key", "/certs/wrong-client.key")
        for name in ("ca.pem", "wrong-ca.pem", "server.key", "server.pem", "client.key", "wrong-client.key", "client.pem", "expired-client.pem"):
            run("docker", "cp", f"{container}:/certs/{name}", str(root / name))
    finally:
        for args in (("docker", "rm", "--force", container), ("docker", "volume", "rm", "--force", volume), ("docker", "image", "rm", image)):
            subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


def main():
    if "--" not in sys.argv:
        raise SystemExit("usage: oauth-container.py -- <test command>")
    command = sys.argv[sys.argv.index("--") + 1:]
    if not command:
        raise SystemExit("missing test command")
    with tempfile.TemporaryDirectory(prefix="craftcloud-oauth-") as temp:
        root = pathlib.Path(temp)
        if shutil.which("openssl"):
            generate_with_host_openssl(root)
        else:
            generate_with_docker(root)
        secret = root / "secret"
        secret.write_text("synthetic-secret-canary", encoding="ascii")
        for fixture in root.iterdir():
            if fixture.is_file():
                fixture.chmod(0o600)
        for case in CASES:
            os.environ["CRAFTCLOUD_OAUTH_TEST_CASE"] = case
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            server.token_count = server.graph_count = 0
            server.seen_jti = set()
            server.authorization_present = server.sender_path_valid = server.save_to_sent_items_false = server.secret_present = server.credential_present = server.graph_scope_valid = False
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(root / "server.pem", root / "server.key")
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            base = f"https://127.0.0.1:{server.server_address[1]}"
            ca_path = root / ("wrong-ca.pem" if case == "untrusted_ca" else "ca.pem")
            client_cert = root / ("expired-client.pem" if case == "certificate_expired" else "client.pem")
            client_key = root / ("wrong-client.key" if case == "certificate_mismatch" else "client.key")
            os.environ.update({"CRAFTCLOUD_OAUTH_TEST_CA": str(ca_path), "CRAFTCLOUD_OAUTH_TEST_TOKEN_URL": base + "/token", "CRAFTCLOUD_OAUTH_TEST_GRAPH_URL": base + "/graph", "CRAFTCLOUD_OAUTH_TEST_SECRET": str(secret), "CRAFTCLOUD_OAUTH_TEST_CERT": str(client_cert), "CRAFTCLOUD_OAUTH_TEST_KEY": str(client_key)})
            try:
                subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
                expected = {"success": (1, 2), "expiry": (2, 2), "401": (2, 2), "429": (1, 1), "403": (1, 1), "404": (1, 1), "unknown": (1, 0), "redirect": (1, 0), "oversized": (1, 0), "untrusted_ca": (0, 0), "certificate_success": (1, 1), "certificate_mismatch": (0, 0), "certificate_expired": (0, 0), "interrupted": (1, 0), "token_429": (1, 0), "token_503": (1, 0)}[case]
                if (server.token_count, server.graph_count) != expected:
                    raise RuntimeError(f"OAuth protocol counters mismatch for {case}")
                protocol = (server.graph_scope_valid, server.sender_path_valid, server.save_to_sent_items_false, server.authorization_present, server.secret_present)
                if (server.token_count and (not server.credential_present or not server.graph_scope_valid)) or (server.graph_count and protocol[1:4] != (True, True, True)):
                    raise RuntimeError(f"OAuth protocol fields mismatch for {case}")
                if case == "certificate_success" and not server.certificate_assertion_valid:
                    raise RuntimeError("OAuth client certificate assertion invalid")
                print(f"{case}: token_requests={server.token_count} graph_requests={server.graph_count} graph_scope_valid={server.graph_scope_valid} sender_path_valid={server.sender_path_valid} save_to_sent_items_false={server.save_to_sent_items_false} authorization_present={server.authorization_present} credential_present={server.credential_present} certificate_assertion_valid={getattr(server, 'certificate_assertion_valid', False)}")
            finally:
                server.shutdown()
                server.server_close()
                thread.join()


if __name__ == "__main__":
    main()
