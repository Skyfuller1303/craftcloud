# ADR-0017: E-Mail-Verifikation wird mit SMTP nachgeholt; P1 ohne Adressverifikation abgeschlossen

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-29); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-29
- **Owner:** Projektverantwortlicher des privaten Prototyps
- **Ergänzt:** ADR-0013 Punkt 5

## Kontext

CC-P01-08 verlangt Einladungen mit E-Mail-Verifikation. PR-007 hat Einladungen ohne SMTP umgesetzt; nach ADR-0013 Punkt 5 gilt der Besitz des Einladungslinks als Nachweis. Das unabhängige Review hat gezeigt, dass daraus ein Restrisiko folgt: Wer einladen darf, kann den eigenen Link selbst annehmen und so ein instanzweites Konto für eine fremde Adresse anlegen („Account-Squatting“). Die echte Person kann spätere Einladungen dann nicht annehmen.

## Entscheidung

1. Die Adressverifikation wird nicht in P1 gebaut, sondern zusammen mit dem SMTP-Versand in einem eigenen PR, spätestens bevor eine Instanz mehreren voneinander unabhängigen Organisationen produktiv dient. Bis dahin gilt ADR-0013 Punkt 5 weiter.
2. P1 gilt mit dieser dokumentierten Lücke als abgeschlossen; CC-P01-08 wird als „umgesetzt ohne Adressverifikation, verschoben per ADR-0017“ geführt.
3. Bis zur Verifikation bleiben die Abmilderungen aus PR-007 Pflicht: Herkunft jedes per Einladung angelegten Kontos (Einladung, Organisation, Einladender), `email_verified_at` bleibt leer, Operator-Übersicht `GET /api/v1/admin/users?unverified=true`, Audit-Zeile bei Kontoerstellung, Sperren betroffener Konten durch den Operator.
4. Der spätere Verifikations-PR muss mindestens liefern: Bestätigungslink an die Adresse vor dem Anlegen eines neuen Kontos, Kennzeichnung bestehender unverifizierter Konten, eine Regel für Einladungen an unverifizierte Konten anderer Organisationen und eine Migration für Altbestände.

## Konsequenzen und Risiken

Der Prototyp eignet sich bis dahin für eine einzelne Organisation oder Organisationen, deren Admins einander vertrauen. Squatting bleibt möglich, ist aber für den Operator sichtbar und behebbar. Das Risiko steht im Threat Model und in `docs/gaps.md`.

## Betrachtete Alternativen

SMTP-Verifikation sofort vor P2 (mehr Infrastruktur, verzögert P2); bis zu SMTP nur Owner/Admins Einladungen an Adressen ohne Konto erlauben (verrät Kontoexistenz an Workspace-Admins und schränkt ADR-0013 Punkt 7 ein).

## Review und Revision

Vor P3 bzw. vor dem ersten Mehrmandantenbetrieb erneut prüfen.
