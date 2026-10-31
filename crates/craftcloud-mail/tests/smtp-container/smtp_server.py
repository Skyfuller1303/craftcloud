import asyncio
import os
import ssl
import subprocess


ROOT = "/test"
DATA_COUNT = 0


def record(event):
    with open(f"{ROOT}/protocol.log", "a", encoding="ascii") as protocol:
        protocol.write(f"{event}\n")


async def handle(reader, writer, context):
    secure = False
    writer.write(b"220 localhost ESMTP ready\r\n")
    await writer.drain()
    while line := await reader.readline():
        verb = line.decode("ascii", "replace").strip().split(" ", 1)[0].upper()
        record(verb)
        if verb in ("EHLO", "HELO"):
            writer.write(b"250-localhost\r\n250-STARTTLS\r\n250 SIZE 1000000\r\n")
        elif verb == "STARTTLS":
            writer.write(b"220 begin TLS\r\n")
            await writer.drain()
            try:
                await writer.start_tls(context)
            except Exception as error:
                reason = getattr(error, "reason", "unknown")
                record(f"TLS_FAIL_{type(error).__name__}_{reason}")
                writer.close()
                return
            secure = True
            record("TLS_OK")
        elif verb in ("MAIL", "RCPT"):
            response = b"250 ok\r\n" if secure else b"530 5.7.0 STARTTLS required\r\n"
            record(f"{verb}_{response[:3].decode('ascii')}")
            writer.write(response)
        elif verb == "DATA":
            if not secure:
                record("DATA_530")
                writer.write(b"530 5.7.0 STARTTLS required\r\n")
                await writer.drain()
                continue
            writer.write(b"354 end with dot\r\n")
            await writer.drain()
            while await reader.readline() not in (b".\r\n", b".\n", b""):
                pass
            global DATA_COUNT
            DATA_COUNT += 1
            if DATA_COUNT == 1:
                writer.write(b"451 4.3.0 temporary test failure\r\n")
                record("DATA_451")
            else:
                writer.write(b"250 2.0.0 accepted\r\n")
                record("DATA_250")
        elif verb == "QUIT":
            writer.write(b"221 bye\r\n")
            await writer.drain()
            break
        else:
            writer.write(b"250 ok\r\n")
        await writer.drain()
    writer.close()
    await writer.wait_closed()


async def main():
    os.makedirs(ROOT, exist_ok=True)
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
        "-keyout", f"{ROOT}/ca-key.pem", "-out", f"{ROOT}/ca.pem",
        "-sha256", "-days", "1", "-subj", "/CN=AC09-test-CA",
        "-addext", "basicConstraints=critical,CA:TRUE",
        "-addext", "keyUsage=critical,keyCertSign,cRLSign",
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run([
        "openssl", "req", "-newkey", "rsa:2048", "-nodes",
        "-keyout", f"{ROOT}/server-key.pem", "-out", f"{ROOT}/server.csr",
        "-subj", "/CN=localhost",
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with open(f"{ROOT}/server-ext.cnf", "w", encoding="ascii") as extensions:
        extensions.write("basicConstraints=critical,CA:FALSE\n")
        extensions.write("keyUsage=critical,digitalSignature,keyEncipherment\n")
        extensions.write("extendedKeyUsage=serverAuth\n")
        extensions.write("subjectAltName=DNS:localhost\n")
    subprocess.run([
        "openssl", "x509", "-req", "-in", f"{ROOT}/server.csr",
        "-CA", f"{ROOT}/ca.pem", "-CAkey", f"{ROOT}/ca-key.pem",
        "-CAcreateserial", "-out", f"{ROOT}/server-cert.pem",
        "-days", "1", "-sha256", "-extfile", f"{ROOT}/server-ext.cnf",
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(f"{ROOT}/server-cert.pem", f"{ROOT}/server-key.pem")
    server = await asyncio.start_server(
        lambda reader, writer: handle(reader, writer, context), "0.0.0.0", 2525
    )
    async with server:
        await server.serve_forever()


asyncio.run(main())
