# 12 — App-Cloud: Briefings für Craft-Apps, Freigaben, FigCraft-Live und Launcher

Stand 2026-10-30. Dieser Ordner beschreibt den nächsten Ausbau von CraftCloud aus Sicht der Nutzer: Craft-Apps speichern ihre Dateien in der Cloud, Personen geben Dateien einander zum Bearbeiten frei, FigCraft-Livelinks funktionieren rund um die Uhr, und der Craft Launcher bekommt ein Nutzerkonto mit Dateibrowser. Jedes Briefing ist so geschrieben, dass eine Codex-Instanz es ohne weiteren Kontext umsetzen kann und am Ende einen `REPORT.md` abliefert.

## Zielbild

1. **Speichern aus den Apps.** PhotoCraft, VectorCraft, FigCraft und spätere Craft-Apps haben „In CraftCloud speichern…“ und „Aus CraftCloud öffnen…“. Gespeichert wird das native Format der App (`.pcraft`, `.psd`, `.figcraft`, `.fig`, `.svg`, `.ai`, …) als unveränderliche Version mit Konfliktschutz. Die lokale Datei bleibt immer erhalten; ohne Cloud verhält sich jede App genau wie heute.
2. **Freigeben und gemeinsam bearbeiten.** Eine Person gibt eine Datei oder ein Projekt im Webpanel oder im Launcher an andere Personen frei (lesen oder bearbeiten). Solange jemand die Datei in einer App geöffnet hat, ist sie für andere gesperrt (Bearbeitungssperre mit Ablauf); andere können sie lesen und nach dem Freiwerden bearbeiten und neu speichern. FigCraft-Dateien sind ausgenommen, weil FigCraft Live-Coworking hat.
3. **FigCraft-Livelinks rund um die Uhr.** CraftCloud betreibt einen FigCraft-Server als Begleitdienst. Wer Bearbeitungsrecht auf eine FigCraft-Datei hat, bekommt einen persönlichen Livelink; der Link funktioniert dauerhaft, solange die Freigabe besteht, und wird beim Entzug sofort ungültig.
4. **Konto im Launcher.** Im Launcher meldet man sich mit E-Mail und Passwort (und, falls eingerichtet, zweitem Faktor) bei der CraftCloud-Instanz des Unternehmens an, sieht sein Profil, seine Dateien und Freigaben und öffnet Dateien direkt in der passenden App.
5. **Zwei-Faktor-Anmeldung.** TOTP mit Wiederherstellungscodes, auf Wunsch der Organisation verpflichtend; gilt im Web, im Launcher und in den Apps.

## Briefings

| ID | Briefing | Repository | Hängt ab von | Prüfung |
|---|---|---|---|---|
| AC-01 | [Desktop-Anmeldung und Geräte-Sitzungen](ac-01-desktop-sign-in.md) | CraftCloud | — | Opus-Review (Sicherheit) |
| AC-02 | [Zwei-Faktor-Anmeldung (TOTP)](ac-02-two-factor.md) | CraftCloud | AC-01 | Opus-Review (Sicherheit) |
| AC-03 | [Freigaben an Personen und Bearbeitungssperre](ac-03-shares-and-edit-locks.md) | CraftCloud | — (AC-01 für Geräte-Sperren) | Opus-Review (Sicherheit) |
| AC-04 | [Client-SDK und Cloud-Dateidialog](ac-04-client-sdk-and-picker.md) | CraftCloud | AC-01, AC-03 | Review |
| AC-05 | [Cloud-Speichern in den Craft-Apps](ac-05-app-integration.md) | PhotoCraft, FigCraft, VectorCraft (lokal) | AC-04 | Review |
| AC-06 | [FigCraft-Livelinks über CraftCloud](ac-06-figcraft-live-hosting.md) | CraftCloud (+ FigCraft-Spike) | AC-03 | Opus-Review (Sicherheit) |
| AC-07 | [Launcher-Konto und Dateibrowser](ac-07-launcher-account.md) | craft-launcher (lokal) | AC-01, AC-02, AC-03, AC-04 | Review |
| AC-08 | [Admin-Konsole](ac-08-admin-console.md) | CraftCloud | AC-01, AC-03, AC-02 | Opus-Review (Sicherheit) |
| OPS-01 | [Installation auf eigenem Server und Releases](ops-01-install-and-releases.md) | CraftCloud | — | Review (Betrieb, Geheimnisse) |

Empfohlene Reihenfolge: AC-01 und AC-03 parallel (verschiedene Dateien, siehe jeweilige Zuständigkeit), danach AC-02 und AC-04, dann AC-05, AC-06 und AC-07. AC-05 und AC-07 arbeiten in fremden Repositories nur lokal. Stand 2026-10-30: AC-01 und AC-03 sind gemerged (main 73a06bd), AC-02 und AC-04 laufen. OPS-01 kann jederzeit parallel laufen, weil es kaum Dateien der anderen Briefings berührt; AC-08 folgt nach AC-02.

## Feste Entscheidungen

Diese Punkte gelten für alle Briefings. Sie sind im [Entscheidungslog](../09-decisions/decision-log.md) unter 2026-10-30 eingetragen; jedes Briefing legt das zugehörige ADR als ersten Schritt an.

- **Server bleibt die Autorität.** Apps und Launcher sind Clients derselben API; jede Berechtigung wird bei jeder Anfrage im zentralen `can()` geprüft. UI-Sichtbarkeit ist kein Schutz.
- **Desktop-Anmeldung mit E-Mail und Passwort ist erlaubt** (Wunsch des Projektverantwortlichen). Sie läuft nur über TLS (Ausnahme: Loopback), liefert eine eigene Geräte-Sitzung mit kurzlebigem Zugriffstoken und rotierendem Erneuerungstoken und legt das Erneuerungstoken im Schlüsselbund des Betriebssystems ab, nie im Klartext in einer Einstellungsdatei. Das ersetzt die frühere Linie aus CC-F17 („Launcher ist keine Auth-Autorität“) teilweise: Der Launcher darf ein eigenes Konto halten, aber keine Tokens an Apps weiterreichen. Jede App meldet sich selbst an.
- **Keine Synchronisation im Hintergrund.** Speichern und Öffnen geschehen auf ausdrücklichen Befehl. Kein Ordner-Sync, keine automatische Übernahme fremder Versionen.
- **Lokale Datei zuerst.** Jede Cloud-Aktion schreibt zuerst lokal; schlägt die Cloud fehl, bleibt die lokale Datei vollständig und die App zeigt, was zu tun ist.
- **Konflikte werden nie still aufgelöst.** Bestehender Mechanismus `If-Match` / 412 `VERSION_CONFLICT` mit „Beide behalten“, „Überschreiben“, „Abbrechen“ (ADR-0009).
- **Bearbeitungssperre ist eine Lease**, kein dauerhafter Zustand: Ablauf nach 5 Minuten ohne Herzschlag, Herzschlag alle 60 Sekunden, Aufheben durch die haltende Person oder erzwungen durch Workspace-Admin und höher (mit Audit). Für `app = figcraft` gibt es keine Sperre.
- **Freigaben an Personen gelten innerhalb der Organisation.** Wer nicht Mitglied ist, wird wie bisher eingeladen (Rolle Gast folgt in AC-03 als eigene Org-Rolle mit Zugriff nur auf freigegebene Objekte). Öffentliche Links ohne Anmeldung bleiben CC-F18 und sind hier nicht enthalten.
- **FigCraft wird nicht nachgebaut.** CraftCloud betreibt den unveränderten `figcraft-server` als Begleitdienst und verwaltet dessen Links mit dem Serverschlüssel (Variante C aus [figcraft-adapter](../04-integrations/figcraft-adapter.md), vorerst ohne Protokolländerung). Eine native Prüfung von CraftCloud-Berechtigungen im FigCraft-Protokoll (Variante B) ist ein späterer Schritt mit den FigCraft-Maintainern.
- **2FA:** TOTP nach RFC 6238 (SHA-1, 6 Stellen, 30 s, ±1 Schritt), 10 Wiederherstellungscodes (nur als Hash gespeichert), TOTP-Geheimnis verschlüsselt mit einem Serverschlüssel aus einer Datei. WebAuthn/Passkeys später.

## Abgrenzung zu parallelen Initiativen

[overlap-map](../00-product/overlap-map.md) beschreibt zwei Vorhaben in der Craft-Organisation, die sich mit diesem Ordner überschneiden: CollabCraft (OIDC, Launcher als Schlüsselbund der Suite, Öffnen/Speichern/Teilen pro App) und CraftHub (Konten über FigCraft-Protokoll v3). Diese Briefings bauen bewusst nur lokal und in CraftCloud. Ob und wie Änderungen in `storytold/*`-Repositories eingereicht werden, entscheidet der Projektverantwortliche gemeinsam mit den jeweiligen Maintainern; kein Briefing erlaubt Veröffentlichung.

## Arbeitsweise für Codex

Gilt für jedes Briefing; das Briefing selbst nennt Worktree, Branch, erlaubte Dateien und Abnahme.

1. **Ort:** Der Koordinator legt vorher einen Git-Worktree an (Name und Branch stehen im Briefing). Nur dort arbeiten. Kein neuer Clone, kein anderer Checkout. In fremden Repositories (PhotoCraft, FigCraft, Launcher) ebenfalls nur im genannten Worktree.
2. **Nicht veröffentlichen:** keine Commits, Pushes, PRs, Issues, Kommentare. Das macht der Koordinator nach Abnahme.
3. **Lesen zuerst:** `AGENTS.md` des Repos, dieses README, das Briefing vollständig, die verlinkten Feature-Briefings (`docs/03-features/`), `docs/05-security/threat-model.md`, `docs/05-security/permission-matrix.md`, `docs/02-architecture/api-conventions.md`.
4. **Builds und Tests:** Alle Cargo- und Node-Läufe über den speicherbegrenzten Launcher, nie über eine PowerShell-Zwischenschicht:
   `python -I D:\ArtCraft\output\figcraft-pr634-review\run-memory-limited.py --gib 8 --json-log <log.json> -- <exe> <args>`
   mit `CARGO_HOME=D:\ArtCraft\.tools\rust\cargo`, `RUSTUP_HOME=D:\ArtCraft\.tools\rust\rustup`, `CARGO_BUILD_JOBS=2`, `CRAFTCLOUD_NODE_DIR=D:\ArtCraft\.tools\node`, `npm_config_cache=D:\ArtCraft\.tools\node-cache`. Datenbanktests mit `cargo xtask db-test` (Docker Desktop läuft; eigene Container eindeutig benennen und am Ende entfernen; `craftcloud-try-pg` und Port 8080 gehören dem Projektverantwortlichen und werden nicht angefasst).
5. **Keine neuen Abhängigkeiten ohne Begründung im Bericht;** jede neue Crate oder jedes npm-Paket mit Version, Lizenz und Grund. Keine Installation globaler Werkzeuge.
6. **Unklare Designfragen:** nicht raten. Die empfohlene, sicherere Option wählen, im Bericht unter „Entscheidungen“ begründen und weiterarbeiten. Nur bei echten Blockern (fehlende Information, die das Ergebnis grundsätzlich ändert) anhalten und im Bericht beschreiben.
7. **Keine Geheimnisse** in Logs, Tests, Bericht oder Commits. Keine personenbezogenen Daten außer synthetischen.
8. **Prozesse** nur über die eigene PID beenden, nie `taskkill /IM`. Kein `git clean -fdx`, kein `wsl --shutdown`, keine Kopien fremder `target`-Ordner.
9. **Am Ende** den Debug-Cache des eigenen Worktrees mit `cargo clean --profile dev` aufräumen (Pfad prüfen) und den freien Platz auf D: vorher/nachher im Bericht nennen.

## Pflichtbericht `REPORT.md`

Jedes Briefing endet mit einem Bericht unter `D:\ArtCraft\output\craftcloud-codex\<ID>\REPORT.md` (Ordner anlegen, falls er fehlt; zusätzliche Logs daneben). Gliederung, auf Deutsch, normale Sätze:

1. **Ergebnis in drei Sätzen:** was jetzt funktioniert, was nicht.
2. **Geänderte Dateien:** Liste mit einer Zeile Zweck je Datei.
3. **Ausgeführte Befehle:** Befehl, Exitcode, Logpfad. Nicht ausgeführte Prüfungen ausdrücklich als „nicht ausgeführt“ mit Grund. Nie „grün“ behaupten, ohne es ausgeführt zu haben.
4. **Abnahmekriterien:** jede Zeile aus dem Abschnitt „Abnahme“ des Briefings mit erfüllt / nicht erfüllt / teilweise und Beleg (Testname, Datei:Zeile, Log).
5. **Entscheidungen:** jede selbst getroffene Designentscheidung mit Begründung und verworfener Alternative (der Koordinator überträgt sie ins Entscheidungslog).
6. **Sicherheit:** neue Angriffsflächen, wie sie abgesichert sind, was offen bleibt.
7. **Abweichungen** vom Briefing mit Grund.
8. **Offene Punkte und Folgearbeit.**
9. **Aufräumen:** entfernte Container/Volumes, gecleante Caches, freier Platz vorher/nachher.

Danach prüft der Koordinator den Diff, führt `cargo xtask ci` und `cargo xtask db-test` selbst aus, lässt sicherheitsrelevante Arbeit unabhängig reviewen und committet, pusht und merged.
