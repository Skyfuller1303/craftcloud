# ADR-0013: Produktentscheidungen für Identität und Zugang in P1

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-28); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

CC-F01 bis CC-F05 lassen mehrere Produkt- und Sicherheitsfragen offen, die vor PR-004 bis PR-007 entschieden sein müssen: Zustellung des Bootstrap-Tokens, Kontoerstellung, Organisationserstellung, Sitzungsdauer, Einladungen und Passwort-Wiederherstellung ohne Mailserver.

## Entscheidung

1. **Bootstrap-Token nur per Secret-Datei.** Der Operator legt den Token in `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE` ab. Der Server erzeugt keinen Token und gibt nie einen auf Konsole oder Log aus. Nach erfolgreichem Setup ist der Setup-Pfad dauerhaft geschlossen.
2. **Organisationen erstellt nur der Instanz-Operator.** Keine Self-Service-Erstellung im MVP. Damit ist die instanzweite Eindeutigkeit von Org-Slugs kein Enumerationsrisiko für normale Nutzer.
3. **Konten entstehen nur über Einladung** (bzw. Bootstrap für den ersten Operator). Keine offene Registrierung.
4. **Sitzungsdauer:** Standard 8 h Inaktivität und 7 Tage absolut, konfigurierbar innerhalb fester Grenzen.
5. **Einladungen ohne SMTP:** Besitz des Einladungslinks gilt im MVP als Nachweis (einmalig, kurzlebig, mindestens 128 Bit Zufall, nur Hash gespeichert). Der Admin übergibt den Link selbst. Eine SMTP-gestützte Verifikation folgt in einem späteren PR. Diese Einschränkung wird im Threat Model geführt.
6. **Passwort-Wiederherstellung:** Ein Org-Admin oder der Operator erzeugt einen einmaligen Reset-Link und übergibt ihn; dabei werden alle Sitzungen des Nutzers beendet. Zusätzlich gibt es eine Offline-CLI-Wiederherstellung für den Operator auf dem Server.

### Ergänzung (2026-10-28)

7. **Workspace-Admins dürfen einladen**, aber nur in ihren eigenen Workspace und höchstens mit der Rolle Editor. Das neue Konto wird Org-Mitglied mit Rolle `member`. Org-Admins können das pro Organisation abschalten.
8. **Editor-Standardrechte:** Projekte anlegen ja, Dateien löschen ja (nur Soft-Delete mit Wiederherstellungsfenster). Ein späterer Workspace-Schalter darf das einschränken.
9. **Link-Gültigkeit:** Einladungslinks 72 h, Reset-Links 1 h; beide einmalig und widerrufbar.
10. **Zwei-Faktor (TOTP)** kommt nach P3 als eigener PR mit Recovery-Codes; bis dahin als bekannte Lücke dokumentiert.

## Konsequenzen und Risiken

Ohne E-Mail-Verifikation kann ein abgefangener Einladungs- oder Reset-Link ein Konto übernehmen. Gegenmaßnahmen: kurze Gültigkeit, einmalige Verwendung, Widerruf, Audit-Eintrag, keine Tokens in Logs oder Referer. Self-Service-Organisationen und offene Registrierung erfordern später eigene ADRs mit Missbrauchsschutz.

## Betrachtete Alternativen

Token einmalig auf der Konsole ausgeben (landet in Container-Logs); offene Registrierung als Option; SMTP-Verifikation bereits in P1 (mehr Infrastruktur); Wiederherstellung nur per CLI (kein Web-Self-Service).

## Review und Revision

Vor einer Übergabe an Maintainer gemeinsam mit dem Threat Model prüfen. SMTP-Verifikation als eigener Folge-PR mit neuer ADR.
