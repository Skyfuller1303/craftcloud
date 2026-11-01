"""Ephemeral, synthetic OIDC test provider. Never used by production code.

Runs in an isolated Python container. The small RSA implementation exists only
in this interoperability fixture; production verification uses openidconnect.
No passwords, authorization codes, claims or tokens are logged.
"""
import base64, hashlib, json, math, secrets, time, uuid
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit


def b64(value):
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode()


def integer(value):
    return b64(value.to_bytes((value.bit_length()+7)//8, "big"))


def prime():
    while True:
        n = secrets.randbits(1024) | (1 << 1023) | 1
        if any(n % p == 0 for p in [3,5,7,11,13,17,19,23,29,31,37,41,43,47]):
            continue
        odd, power = n-1, 0
        while odd % 2 == 0:
            odd //= 2
            power += 1
        for _ in range(12):
            x = pow(secrets.randbelow(n-3)+2, odd, n)
            if x in (1, n-1):
                continue
            for _ in range(power-1):
                x = pow(x, 2, n)
                if x == n-1:
                    break
            else:
                break
        else:
            if math.gcd(65537, n-1) == 1:
                return n


def key(kid):
    while True:
        p,q = prime(),prime()
        n = p*q
        if p != q and n.bit_length() == 2048:
            return {"kid":kid, "n":n, "d":pow(65537,-1,(p-1)*(q-1))}


KEYS = [key("first")]
CONFIG = {"mode":"success", "subject":"employee-stable-subject", "email":"employee@example.test", "mfa":True, "key":0}
CODES = {}
STATS = {"token_calls":0,"jwks_calls":0}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def output(self, status, value, location=None):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(data)

    @property
    def issuer(self):
        return "http://"+self.headers["Host"]+"/realm"

    def do_GET(self):
        parsed = urlsplit(self.path)
        if parsed.path == "/realm/.well-known/openid-configuration":
            self.output(200,{"issuer":self.issuer,"authorization_endpoint":self.issuer+"/authorize","token_endpoint":self.issuer+"/token","jwks_uri":self.issuer+"/jwks","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"],"token_endpoint_auth_methods_supported":["client_secret_basic"],"code_challenge_methods_supported":["S256"]})
        elif parsed.path == "/realm/jwks":
            STATS["jwks_calls"] += 1
            k = KEYS[CONFIG["key"]]
            self.output(200,{"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":k["kid"],"n":integer(k["n"]),"e":integer(65537)}]})
        elif parsed.path == "/realm/authorize":
            q = parse_qs(parsed.query)
            if q.get("response_type") != ["code"] or q.get("code_challenge_method") != ["S256"]:
                self.output(400,{"error":"invalid_request"})
                return
            code = uuid.uuid4().hex
            CODES[code] = {"nonce":q["nonce"][0],"challenge":q["code_challenge"][0],"client_id":q["client_id"][0],"redirect_uri":q["redirect_uri"][0],"created":time.time()}
            target = q["redirect_uri"][0]+"?"+urlencode({"code":code,"state":q["state"][0],"iss":self.issuer})
            self.output(302,{},target)
        elif parsed.path == "/stats":
            self.output(200,STATS)
        else:
            self.output(404,{"error":"not_found"})

    def do_POST(self):
        size = int(self.headers.get("Content-Length","0"))
        if size > 8192:
            self.output(413,{"error":"too_large"})
            return
        raw = self.rfile.read(size)
        if self.path == "/control":
            value = json.loads(raw)
            if set(value)-{"mode","subject","email","mfa","rotate"}:
                self.output(400,{"error":"invalid_control"})
                return
            if value.pop("rotate",False):
                if len(KEYS) == 1:
                    KEYS.append(key("rotated"))
                CONFIG["key"] = 1-CONFIG["key"]
            CONFIG.update(value)
            self.output(200,{"ready":True})
        elif self.path == "/realm/token":
            STATS["token_calls"] += 1
            q = parse_qs(raw.decode())
            code = CODES.pop(q.get("code",[""])[0],None)
            auth = self.headers.get("Authorization","")
            if not code or time.time()-code["created"] > 600 or auth != "Basic "+base64.b64encode((code["client_id"]+":synthetic-idp-secret").encode()).decode() or q.get("redirect_uri") != [code["redirect_uri"]] or b64(hashlib.sha256(q.get("code_verifier",[""])[0].encode()).digest()) != code["challenge"]:
                self.output(400,{"error":"invalid_grant"})
                return
            now = int(time.time())
            mode = CONFIG["mode"]
            claims = {"iss":self.issuer,"sub":CONFIG["subject"],"aud":code["client_id"],"iat":now,"exp":now+300,"nonce":code["nonce"],"email":CONFIG["email"],"email_verified":True,"name":"Synthetic Employee","amr":["mfa"] if CONFIG["mfa"] else ["pwd"]}
            if mode == "wrong_issuer": claims["iss"] = "https://foreign.example.test"
            if mode == "wrong_audience": claims["aud"] = "another-client"
            if mode == "expired": claims["exp"] = now-120
            if mode == "wrong_nonce": claims["nonce"] = "another-nonce"
            if mode == "future_iat": claims["iat"] = now+600
            if mode == "unverified": claims["email_verified"] = False
            if mode == "foreign_domain": claims["email"] = "employee@foreign.test"
            k = KEYS[CONFIG["key"]]
            header = {"alg":"RS256","typ":"JWT","kid":k["kid"]}
            signing = b64(json.dumps(header,separators=(",",":")).encode())+"."+b64(json.dumps(claims,separators=(",",":")).encode())
            digest_info = bytes.fromhex("3031300d060960864801650304020105000420")+hashlib.sha256(signing.encode()).digest()
            message = b"\x00\x01"+b"\xff"*(256-len(digest_info)-3)+b"\x00"+digest_info
            signature = pow(int.from_bytes(message,"big"),k["d"],k["n"]).to_bytes(256,"big")
            if mode == "wrong_signature": signature = bytes([signature[0]^1])+signature[1:]
            self.output(200,{"access_token":"synthetic-provider-access","token_type":"Bearer","expires_in":300,"id_token":signing+"."+b64(signature)})
        else:
            self.output(404,{"error":"not_found"})


HTTPServer(("0.0.0.0",8081),Handler).serve_forever()
