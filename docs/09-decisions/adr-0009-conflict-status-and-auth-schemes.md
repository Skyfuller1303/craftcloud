# ADR-0009: Konfliktstatus 412 und vorläufige Auth-Schemata im Contract

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

Der Blueprint war uneinheitlich: Persona C nannte 409, `sync-state-machine.md`, CC-F08 und das P2-Gate nannten 412, `api-conventions.md` ließ „409/412“ offen. Außerdem nutzte das Commit-Beispiel `Bearer <device-token>`, obwohl der Desktop-Login (D13) offen ist, und der OpenAPI-Entwurf kannte kein Schema für Browser-Sessions.

## Entscheidungsvorschlag

1. Scheitert eine `If-Match`-Vorbedingung gegen den Remote-Head, antwortet der Server mit **412 Precondition Failed** und Code `VERSION_CONFLICT` plus `details.current_version_id`. Das entspricht der HTTP-Semantik bedingter Requests (RFC 9110, Abschnitt 13.1.1).
2. **409 Conflict** bleibt reserviert für Wiederverwendung eines `Idempotency-Key` mit abweichendem Payload und für Zustandskonflikte ohne Vorbedingung (z. B. zweiter Bootstrap).
3. Der Contract führt `cookieAuth` (Browser-Session + CSRF-Header) als einziges MVP-Schema. `bearerAuth` bleibt als Platzhalter markiert, bis ein eigener ADR zum Desktop-Flow (D13) entschieden ist.

## Offene Punkte (nicht durch diesen ADR entschieden)

- HTTP-Framework für `craftcloud-server` (früher stand in `deployment.md` „Actix“ ohne Begründung).
- Endgültiger Name des Session-Cookies und des CSRF-Headers.

## Konsequenzen und Risiken

Clients müssen 412 als Konflikt und nicht als generischen Fehler behandeln. Ein späterer Bearer-Flow erfordert eine additive Contract-Änderung.

## Betrachtete Alternativen

409 für alle Konflikte (einfacher, aber vermischt Vorbedingung und Idempotenz); Bearer sofort für alle Clients (widerspricht D13 und `api-conventions.md`).

## Review und Revision

Im Phase-0-Review gemeinsam mit ADR-0004 prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen.
