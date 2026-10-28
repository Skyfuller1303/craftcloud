# Sicherheitsanforderungen / Implementierungs-Checkliste

## Auth

- Argon2id (aktuelle Parameter reviewen), Passwortpolicy und Sperrlogik ohne dauerhafte Account-Lockout-DoS.
- Crypto RNG für Session/Invite-/Bootstrap-Tokens; nur Hash der bearer tokens speichern; constant-time checks wo relevant.
- Websession: HttpOnly, Secure, SameSite, CSRF-Token, Origin/Host validation, Session Fixation verhindern, Logout/Password-Change invalidiert relevante Sessions.
- Native Device Flow erst nach gesondertem ADR, OIDC nicht automatisch Voraussetzung für lokale Accounts.

## Daten/API

- Serverseitiger Permission Check vor jeder Dateioperation und vor Commit nochmal.
- DB-Abfragen nach tenant scope; FK constraints, unique index and transactional preconditions.
- Keine bekannten Pfad-/Objektnamen direkt als Datei- oder S3-Key verwenden.
- Request-Body-, Header-, JSON-, Upload-, Dateizahl- und Datenbank-Pool-Limits; kontrollierte Timeouts; Rate limiting pro Route/Actor.
- No unsafe, avoid production panics, no unchecked untrusted indexing/arithmetics.
- Strict CORS allowlist, no wildcards with credentials; CSP for admin frontend; no leaking tokens into URLs or logging.

## Betrieb

- TLS terminate trusted reverse proxy, Proxy Trust Configuration and HTTPS Public URL mandatory for remote use.
- Container non-root, minimum filesystem capabilities, readonly mount where practicable, secrets as files/secret manager.
- No default admin password; secure bootstrap, once only.
- Backups use encrypted transport/at-rest as configured, signed/checksummed manifest, verified restores.
- Security advisories/critical dependencies and scanning defined; private vulnerability reporting.

## Release Gate

All threat-model abuse cases automated or manually reproduced, CSRF/IDOR tested, permissions reviewed, secret logging negative test, backup/restore run, TLS/proxy config checked, operator runbook validated. No declaration of security certification without independent audit.
