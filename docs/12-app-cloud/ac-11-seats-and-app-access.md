# AC-11 — Plätze und App-Zuweisung

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac11`, Branch `feat/ac-11-seats-apps` (Basis: aktueller `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-11\REPORT.md`. **Migration:** die nächste freie Nummer, die der Koordinator beim Start nennt. **Review:** unabhängig, sicherheitsrelevant (Zugriffssteuerung je App).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac11 (Branch feat/ac-11-seats-apps).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-11-seats-and-app-access.md vollständig
und setze AC-11 genau danach um. Halte die Arbeitsweise aus dem README ein.
Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-11\REPORT.md im vorgegebenen Format.
```

## Ziel

Wie in einer Admin-Konsole für Kreativ-Software weist ein Organisations-Admin Personen die Craft-Apps zu, die sie mit der Cloud nutzen dürfen (zum Beispiel PhotoCraft und FigCraft, aber nicht VectorCraft), und sieht, wie viele Plätze belegt sind. Der Betreiber kann je Organisation eine Höchstzahl an Plätzen festlegen. **Es gibt keine Abrechnung, keine Zahlungsdaten und keine Lizenzschlüssel:** Die Craft-Apps bleiben frei nutzbar; AC-11 steuert nur den Zugang zur Cloud der eigenen Instanz.

## Ist-Stand (bei Start prüfen)

- Geräte-Sitzungen (AC-01) tragen die App-Kennung (`app` beim Geräte-Login), Dateien tragen `app` und `format`.
- Mitgliedschaften haben Rolle und Status; Gäste (AC-03) haben nur Zugriff über Freigaben.
- Admin-Konsole (AC-08/AC-08b) mit Personen-Tabelle, Mehrfachaktionen und Übersicht.

## Entscheidungen (fest)

1. **App-Katalog:** feste, im Server gepflegte Liste bekannter Craft-Apps (Kennung, Anzeigename, Dateiformate), erweiterbar über eine Konfigurationsdatei des Betreibers. Unbekannte App-Kennungen beim Geräte-Login werden abgelehnt, sobald die Organisation die App-Zuweisung aktiviert hat.
2. **Zuweisung:** Tabelle `member_app_access (org_id, user_id, app)`. Die Organisation wählt einen Modus: „alle Apps für alle Mitglieder“ (Standard, heutiges Verhalten) oder „nur zugewiesene Apps“. Owner und Admins haben immer alle Apps. Gäste brauchen keine Zuweisung; für sie gelten nur Freigaben, und sie belegen keinen Platz.
3. **Durchsetzung:** im zentralen `can()` als zusätzliche Bedingung für Geräte-Sitzungen einer App: Öffnen, Speichern und Sperren einer Datei sind nur mit einer Geräte-Sitzung erlaubt, deren App der Person zugewiesen ist. Das Webpanel (Browser-Sitzung) bleibt für Ansehen, Herunterladen, Hochladen und Freigeben unabhängig von der Zuweisung nutzbar. Entzug einer App beendet die Geräte-Sitzungen dieser Person für diese App und gibt ihre Sperren frei (Audit).
4. **Plätze:** Ein Platz ist ein aktives Mitglied (nicht Gast, nicht deaktiviert, nicht eingeladen). Der Operator setzt optional `seat_limit` je Organisation (leer = unbegrenzt). Beim Erreichen des Limits schlagen neue Einladungen und das Reaktivieren mit `409 SEAT_LIMIT_REACHED` fehl; bestehende Mitglieder sind nie betroffen. Prüfung unter der Organisationssperre (Lock-Reihenfolge beachten), damit parallele Einladungen das Limit nicht überschreiten.
5. **Webpanel:** Personen-Tabelle bekommt eine Spalte „Apps“ und Mehrfachaktionen „Apps zuweisen“ und „Apps entziehen“ (je Person einzeln wie in AC-08). Neuer Bereich „Apps“ in der Verwaltung: je App Anzahl zugewiesener Personen, aktive Geräte und Speicher der Dateien dieser App. Übersicht zeigt belegte Plätze gegen Limit. Operator setzt das Limit unter „Organisationen“.
6. **Launcher und Apps:** Bei abgelehntem Geräte-Login wegen fehlender Zuweisung antwortet der Server `403 APP_NOT_ASSIGNED`; das SDK bildet das auf einen eigenen Fehler mit verständlichem Text ab („Dein Admin hat dir diese App für die Cloud nicht zugewiesen“). Die App bleibt lokal voll nutzbar.
7. **Nicht vorgesehen:** Bezahlfunktionen, Testzeiträume, Lizenzschlüssel, Funktionssperren in den Apps selbst.

## Erlaubte Dateien

`crates/craftcloud-api/src/**`, `crates/craftcloud-db/**` (Migration), `crates/craftcloud-core/src/**` (Policy, App-Katalog), `crates/craftcloud-client/**` (neuer Fehlercode), `apps/craftcloud-server/src/**`, `apps/craftcloud-admin/src/**`, `contracts/openapi.yaml` und Snapshot, `docs/03-features/cc-f05-roles-and-permissions.md`, `docs/03-features/cc-f10-quotas-and-retention.md`, `docs/05-security/permission-matrix.md`, neues ADR `docs/09-decisions/adr-0027-seats-and-app-access.md`, `docs/gaps.md`.

## Tests (Pflicht)

- Policy: Matrix für Modus × Rolle × zugewiesen/nicht zugewiesen × Browser/Gerät.
- DB/HTTP: Geräte-Login abgelehnt ohne Zuweisung; Entzug beendet Sitzungen und Sperren; Platzlimit bei parallelen Einladungen (nebenläufiger Test) nicht überschritten; Gäste belegen keinen Platz; fremde Organisation 404.
- SDK: `APP_NOT_ASSIGNED` wird korrekt abgebildet.
- Web: Personen-Spalte, Mehrfachaktion mit Teilfehlern, Apps-Bereich, EN/DE mit axe.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Standardmodus ändert nichts am bisherigen Verhalten (Test belegt).
3. Platzlimit hält unter Nebenläufigkeit (Test belegt).
4. Entscheidungen 1–7 umgesetzt oder begründet abweichend; ADR-0027 angelegt.

## Nicht Teil von AC-11

Abrechnung und Zahlungen, Lizenzschlüssel, Nutzungsauswertung pro Funktion, App-Verteilung über den Launcher.
