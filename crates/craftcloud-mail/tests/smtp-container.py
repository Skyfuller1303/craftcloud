"""Run isolated STARTTLS delivery tests in an owned, disposable container."""

import json
import os
import pathlib
import subprocess
import sys
import time
import uuid
import socket


def run(*args, capture=False):
    return subprocess.run(args, check=True, text=True, capture_output=capture)


def require_starttls(port):
    with socket.create_connection(("127.0.0.1", int(port)), timeout=3) as connection:
        stream = connection.makefile("rwb", buffering=0)
        stream.readline()
        stream.write(b"EHLO localhost\r\n")
        while (line := stream.readline()).startswith(b"250-"):
            pass
        stream.write(b"MAIL FROM:<sender@example.invalid>\r\n")
        response = stream.readline()
        if response[:3] != b"530":
            raise RuntimeError(f"Expected SMTP 530 before STARTTLS, received {response[:3]!r}")


def main():
    if "--" not in sys.argv:
        raise SystemExit("usage: smtp-container.py -- <test command>")
    test_command = sys.argv[sys.argv.index("--") + 1:]
    if not test_command:
        raise SystemExit("missing test command")
    tag = uuid.uuid4().hex[:12]
    image = f"craftcloud-ac09-smtp:{tag}"
    container = f"craftcloud-ac09-smtp-{tag}"
    volume = f"craftcloud-ac09-smtp-{tag}"
    context = pathlib.Path(__file__).parent / "smtp-container"
    output_value = os.environ.get("CRAFTCLOUD_AC09_OUTPUT")
    if not output_value:
        raise SystemExit("CRAFTCLOUD_AC09_OUTPUT must point to the AC-09 output directory")
    output = pathlib.Path(output_value).resolve()
    output.mkdir(parents=True, exist_ok=True)
    ca_path = output / f"ac09-smtp-ca-{tag}.pem"
    protocol_path = output / f"ac09-smtp-protocol-{tag}.log"
    try:
        run("docker", "volume", "create", volume, capture=True)
        run("docker", "build", "--file", str(context / "Dockerfile"), "--tag", image, str(context))
        run("docker", "run", "--detach", "--name", container, "--memory", "192m", "--cpus", "0.5", "--publish", "127.0.0.1::2525",
            "--volume", f"{volume}:/test", image, capture=True)
        inspect = run("docker", "inspect", container, capture=True)
        port = json.loads(inspect.stdout)[0]["NetworkSettings"]["Ports"]["2525/tcp"][0]["HostPort"]
        for _ in range(80):
            running = run("docker", "inspect", "--format", "{{.State.Running}}", container, capture=True).stdout.strip()
            if running != "true":
                raise RuntimeError("SMTP test container exited during startup")
            try:
                run("docker", "exec", container, "test", "-s", "/test/ca.pem")
                break
            except subprocess.CalledProcessError:
                time.sleep(0.25)
        else:
            raise RuntimeError("SMTP test container did not create its TLS certificate")
        require_starttls(port)
        run("docker", "cp", f"{container}:/test/ca.pem", str(ca_path))
        environment = os.environ.copy()
        environment.update({
            "CRAFTCLOUD_TEST_SMTP_HOST": "localhost",
            "CRAFTCLOUD_TEST_SMTP_PORT": port,
            "CRAFTCLOUD_TEST_SMTP_CA": str(ca_path),
        })
        completed = subprocess.run(test_command, env=environment, check=False)
        subprocess.run(["docker", "cp", f"{container}:/test/protocol.log", str(protocol_path)], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if completed.returncode:
            with (output / f"ac09-smtp-container-{tag}.log").open("w", encoding="utf-8") as log:
                subprocess.run(["docker", "logs", container], check=False, stdout=log, stderr=subprocess.STDOUT)
            raise SystemExit(completed.returncode)
        events = protocol_path.read_text(encoding="ascii").splitlines()
        required = {"MAIL_530", "TLS_FAIL_SSLError_TLSV1_ALERT_UNKNOWN_CA", "TLS_OK", "DATA_451", "DATA_250"}
        if not required.issubset(events):
            raise RuntimeError(f"SMTP integration protocol did not prove required TLS/retry events: {sorted(required - set(events))}")
    finally:
        subprocess.run(["docker", "cp", f"{container}:/test/protocol.log", str(protocol_path)], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker", "rm", "--force", container], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker", "volume", "rm", "--force", volume], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker", "image", "rm", image], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ca_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
