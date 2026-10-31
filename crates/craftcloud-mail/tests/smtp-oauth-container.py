"""Run isolated SMTP OAuth protocol cases with unique local TLS credentials."""

import base64
import http.server
import importlib.util
import json
import os
import pathlib
import shutil
import socket
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import uuid


CASES = ("success", "expired", "535_refresh", "permanent_535", "334_refresh", "334_permanent", "wrong_sender", "google", "untrusted_ca")
SENDER = "sender@example.invalid"


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def make_credentials(root):
    generator_path = pathlib.Path(__file__).with_name("oauth-container.py")
    spec = importlib.util.spec_from_file_location("craftcloud_oauth_container", generator_path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load OAuth HTTPS certificate generator")
    generator = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(generator)
    generator.generate_with_host_openssl(root)
    run("openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(root / "google.key"))
    run("openssl", "pkey", "-in", str(root / "google.key"), "-pubout", "-out", str(root / "google.pub"))


def verify_google_assertion(root, assertion):
    try:
        head, body, signature = assertion.split(".")
        header = json.loads(base64.urlsafe_b64decode(head + "=="))
        claims = json.loads(base64.urlsafe_b64decode(body + "=="))
        if header.get("alg") != "RS256" or claims.get("sub") != SENDER or claims.get("scope") != "https://mail.google.com/" or claims.get("aud") != "https://oauth2.googleapis.com/token":
            return False
        signed = root / "google.signed"
        sig = root / "google.sig"
        signed.write_text(head + "." + body, encoding="ascii")
        sig.write_bytes(base64.urlsafe_b64decode(signature + "=="))
        subprocess.run(("openssl", "dgst", "-sha256", "-verify", str(root / "google.pub"), "-signature", str(sig), str(signed)), check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return True
    except (ValueError, KeyError, json.JSONDecodeError, subprocess.CalledProcessError):
        return False


class TokenHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        form = urllib.parse.parse_qs(self.rfile.read(size).decode("ascii"), keep_blank_values=True)
        self.server.token_count += 1
        if self.server.case == "google":
            valid = form.get("grant_type") == ["urn:ietf:params:oauth:grant-type:jwt-bearer"] and verify_google_assertion(self.server.root, form.get("assertion", [""])[0])
            self.server.google_assertion_valid &= valid
        else:
            valid = form.get("scope") == ["https://outlook.office365.com/.default"] and form.get("client_secret") == ["synthetic-secret-canary"]
            self.server.outlook_scope_valid &= valid
        body = json.dumps({"access_token": "synthetic-access-token", "token_type": "Bearer", "expires_in": 1 if self.server.case == "expired" else 3600}).encode("ascii")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class SmtpHandler(socketserver.BaseRequestHandler):
    def reply(self, file, text):
        file.write((text + "\r\n").encode("ascii"))
        file.flush()

    def handle(self):
        sock = self.request
        file = sock.makefile("rwb")
        self.reply(file, "220 localhost ESMTP")
        tls = False
        authenticated = False
        while True:
            line = file.readline()
            if not line:
                return
            command, _, argument = line.decode("ascii", "replace").rstrip("\r\n").partition(" ")
            command = command.upper()
            if command in ("EHLO", "HELO"):
                self.reply(file, "250-localhost")
                self.reply(file, "250-" + ("AUTH XOAUTH2" if tls else "STARTTLS"))
                self.reply(file, "250 SIZE 1048576")
            elif command == "STARTTLS" and not tls:
                self.reply(file, "220 Ready to start TLS")
                file.close()
                sock = self.server.context.wrap_socket(sock, server_side=True)
                file = sock.makefile("rwb")
                tls = True
            elif command == "AUTH" and tls:
                mechanism, _, encoded = argument.partition(" ")
                try:
                    decoded = base64.b64decode(encoded, validate=True).decode("utf-8") if mechanism == "XOAUTH2" else ""
                except (ValueError, UnicodeDecodeError):
                    decoded = ""
                claims = decoded.split("\x01")
                user_ok = f"user={SENDER}" in claims
                token_ok = "auth=Bearer synthetic-access-token" in claims
                self.server.xoauth_user_valid &= user_ok
                self.server.xoauth_token_valid &= token_ok
                self.server.auth_count += 1
                reject = self.server.case == "permanent_535" or (self.server.case == "535_refresh" and self.server.auth_count == 1)
                challenge = self.server.case == "334_permanent" or (self.server.case == "334_refresh" and self.server.auth_count == 1)
                if challenge:
                    # Gmail-style failure: a 334 JSON error challenge, then 535 after the client's empty line.
                    self.reply(file, "334 " + base64.b64encode(b'{"status":"400","schemes":"Bearer","scope":"https://mail.google.com/"}').decode("ascii"))
                    reply = file.readline()
                    if reply.strip(b"\r\n") == b"" and reply:
                        self.reply(file, "535-5.7.8 Username and Password not accepted.")
                        self.reply(file, "535 5.7.8 Learn more")
                    else:
                        return
                elif reject:
                    self.reply(file, "535 5.7.8 Authentication credentials invalid")
                elif user_ok and token_ok:
                    authenticated = True
                    self.reply(file, "235 2.7.0 Authentication successful")
                else:
                    self.reply(file, "535 5.7.8 Authentication rejected")
            elif command == "MAIL":
                self.reply(file, "250 2.1.0 Sender accepted" if authenticated else "530 5.7.0 Authentication required")
            elif command == "RCPT":
                self.reply(file, "250 2.1.5 Recipient accepted")
            elif command == "DATA":
                self.reply(file, "354 End data with <CR><LF>.<CR><LF>")
                while file.readline().strip() != b".":
                    pass
                self.reply(file, "250 2.0.0 Queued")
            elif command == "QUIT":
                self.reply(file, "221 2.0.0 Bye")
                return
            else:
                self.reply(file, "250 2.0.0 OK")


class ThreadedTcpServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def serve(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return thread


def main():
    if "--" not in sys.argv:
        raise SystemExit("usage: smtp-oauth-container.py -- <test command>")
    command = sys.argv[sys.argv.index("--") + 1:]
    if not command:
        raise SystemExit("missing test command")
    if not shutil.which("openssl"):
        raise SystemExit("openssl is required; Docker fallback disabled")
    with tempfile.TemporaryDirectory(prefix="craftcloud-smtp-oauth-") as temp:
        root = pathlib.Path(temp)
        make_credentials(root)
        (root / "secret").write_text("synthetic-secret-canary", encoding="ascii")
        (root / "service-account.json").write_text(json.dumps({"client_email": "service@example.invalid", "private_key": (root / "google.key").read_text(encoding="ascii"), "token_uri": "https://oauth2.googleapis.com/token"}), encoding="ascii")
        for fixture in root.iterdir():
            if fixture.is_file():
                fixture.chmod(0o600)
        for case in CASES:
            token = http.server.ThreadingHTTPServer(("127.0.0.1", 0), TokenHandler)
            token.case = case
            token.root = root
            token.token_count = 0
            token.outlook_scope_valid = True
            token.google_assertion_valid = True
            tls_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            tls_context.load_cert_chain(root / "server.pem", root / "server.key")
            token.socket = tls_context.wrap_socket(token.socket, server_side=True)
            token_thread = serve(token)
            smtp = ThreadedTcpServer(("127.0.0.1", 0), SmtpHandler)
            smtp.case = case
            smtp.context = tls_context
            smtp.auth_count = 0
            smtp.xoauth_user_valid = True
            smtp.xoauth_token_valid = True
            smtp_thread = serve(smtp)
            os.environ.update({
                "CRAFTCLOUD_SMTP_OAUTH_CASE": case,
                "CRAFTCLOUD_SMTP_OAUTH_CA": str(root / "ca.pem"),
                "CRAFTCLOUD_SMTP_OAUTH_SMTP_CA": str(root / ("wrong-ca.pem" if case == "untrusted_ca" else "ca.pem")),
                "CRAFTCLOUD_SMTP_OAUTH_TOKEN_URL": f"https://127.0.0.1:{token.server_port}/token",
                "CRAFTCLOUD_SMTP_OAUTH_SMTP_PORT": str(smtp.server_address[1]),
                "CRAFTCLOUD_SMTP_OAUTH_SECRET": str(root / "secret"),
                "CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT": str(root / "service-account.json"),
            })
            try:
                subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                expected = {
                    "success": (1, 1), "expired": (2, 2), "535_refresh": (2, 2),
                    "permanent_535": (2, 2), "334_refresh": (2, 2), "334_permanent": (2, 2), "wrong_sender": (0, 0), "google": (1, 1),
                    "untrusted_ca": (1, 0),
                }[case]
                counts = (token.token_count, smtp.auth_count)
                if counts != expected:
                    raise RuntimeError(f"SMTP OAuth counters mismatch for {case}")
                if case == "google" and not token.google_assertion_valid:
                    raise RuntimeError("Google JWT signature or claims invalid")
                if case not in ("google", "wrong_sender") and token.token_count and not token.outlook_scope_valid:
                    raise RuntimeError("Outlook token scope or credentials invalid")
                if smtp.auth_count and not (smtp.xoauth_user_valid and smtp.xoauth_token_valid):
                    raise RuntimeError("XOAUTH2 decoded user or bearer token invalid")
                print(f"{case}: token_requests={token.token_count} smtp_auth_attempts={smtp.auth_count} outlook_scope_valid={token.outlook_scope_valid} google_assertion_valid={token.google_assertion_valid} xoauth_user_valid={smtp.xoauth_user_valid} xoauth_token_valid={smtp.xoauth_token_valid}")
            finally:
                token.shutdown()
                token.server_close()
                smtp.shutdown()
                smtp.server_close()
                token_thread.join()
                smtp_thread.join()


if __name__ == "__main__":
    main()
