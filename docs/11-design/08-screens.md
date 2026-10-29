# 08 — Ansichten

Alle Ansichten liegen in `apps/craftcloud-admin/src/screens/`. Funktionen, Texte und Reihenfolge der Bedienelemente bleiben; hier steht nur, wie sie aussehen. Elemente siehe [06](06-components.md), Layout siehe [05](05-spacing-and-layout.md).

## Nicht angemeldet (`App.tsx` Rahmen `.frame`)

Kopfleiste mit Wortmarke und Sprache/Thema; Inhalt als zentrierte Karte.

| Ansicht | Datei | Gestaltung |
|---|---|---|
| Laden | `App.tsx` | Karte mit Titel und unbestimmtem Fortschritt (4 px) statt nur Text; Ladetext bleibt als Live-Region. |
| Server nicht erreichbar | `App.tsx` | Karte, Banner `bad`, Button „Erneut versuchen“ (sekundär, volle Breite). |
| Login | `LoginScreen.tsx` | Karte 400 px. Banner (Sitzung beendet = info, Einladung ausstehend = info, Fehler = bad) über den Feldern. Felder E-Mail, Passwort. Primär „Anmelden“ volle Breite. |
| Einrichtung | `SetupScreen.tsx` | Karte 480 px. Beschreibung `--muted`. Felder: Setup-Token (mono), E-Mail, Name, Passwort (Hinweis), Organisationsname, Adressname (Hinweis, mono). Zwischen Konto- und Organisationsfeldern eine Haarlinie mit 8 px Abstand. Primär volle Breite. |
| Einladung annehmen | `InviteScreen.tsx` | Karte 400 px. Felder nur wenn nicht angemeldet. Primär „Einladung annehmen“ volle Breite; darunter „Ich habe schon ein Konto“ als Link-Button, „Einladung verwerfen“ als `danger-quiet` klein, zentriert. |
| Passwort setzen | `ResetPasswordScreen.tsx` | Karte 400 px. Zwei Passwortfelder, Hinweis, Primär volle Breite. Erfolg: Banner good + Primär „Zur Anmeldung“. Abgelehnt: Banner bad + **ebenfalls Primär** „Zur Anmeldung“ (heute sekundär — vereinheitlichen). |

## Angemeldet (`Shell.tsx`)

Kopfleiste und Navigation nach [05](05-spacing-and-layout.md). Hinweise der Shell (Erfolg nach Aktionen) als Banner `good` unter dem Seitenkopf. Lade-/Fehler-/Leerzustände für Organisationen und Workspaces nach [09](09-motion-and-feedback.md). „Weitere Organisationen laden“ / „Weitere Workspaces laden“ als Leise-Buttons direkt unter dem jeweiligen Kontext-Hinweis.

### Übersicht

Faktenliste: Angemeldet als (Name, darunter E-Mail `--muted`, Badge „Instanz-Betreiber“ neutral falls vorhanden), Organisation (Name + Rollen-Badge), Workspace (Name + Rollen-Badge oder Hinweis `--muted`).

### Projekte (`ProjectsScreen.tsx`)

- Seitenkopf: Titel; Primäraktion ist das Anlegen.
- Toolbar: Inline-Formular (Feld „Neuer Projektname“ 280 px + Primär „Projekt anlegen“), rechts Checkbox „Papierkorb anzeigen“. Ohne Schreibrecht: Hinweis `--muted` statt Formular.
- Tabelle: **Name** (Zeilenkopf) · **Geändert**/**Gelöscht** (`numeric`, `--muted`) · **Aktionen**: Dateien (sekundär sm), Umbenennen (sekundär sm), In den Papierkorb (`danger-quiet` sm); im Papierkorb: Wiederherstellen (sekundär sm).
- Leer: EmptyState mit vorhandenem Text. „Mehr laden“ unter der Tabelle.
- Dialoge: Umbenennen `modal--sm` (Feld, Abbrechen | Speichern primär); Papierkorb-Bestätigung `modal--sm` (Abbrechen | In den Papierkorb `danger`).

### Dateien (`FilesScreen.tsx`)

- Toolbar 1: Projekt-Select (Standard-Variante, 280 px), daneben „Weitere Projekte laden“ (leise).
- Toolbar 2: Primär „Datei hochladen“ links, Checkbox „Papierkorb anzeigen“ rechts.
- Tabelle: **Name** (Zeilenkopf) · **Format** (`code`, `col--optional`) · **Größe** (`numeric`; „Noch kein Inhalt“ als Badge neutral) · **Geändert**/**Gelöscht** (`numeric`) · **Aktionen**: Herunterladen (`<a class="button button--sm">`), Versionen, Neue Version, In den Papierkorb (`danger-quiet`); Papierkorb: Wiederherstellen.

### Upload-Dialog (`UploadDialog.tsx`), `modal--md`

| Phase | Gestaltung |
|---|---|
| Auswahl | Dateifeld (Datei-Button-Stil), Felder Dateiname und App (Hinweis), Größe `--muted`. Fuß: Abbrechen | Hochladen (primär). |
| Arbeit | Schrittliste (`.steps`) mit allen Schritten, darunter Fortschritt (Beschriftung „Wird hochgeladen…“, rechts „{done} von {total}“). Fuß: „Upload abbrechen“ (sekundär). Während „Version wird gespeichert…“ deaktiviert (bestehendes Verhalten). |
| Konflikt | Titel bleibt, Erklärtext, Aufzählung der Optionen als Liste mit Haarlinien statt Punkten (Option fett, Erklärung `--muted`). Fuß: Abbrechen | Überschreiben (sekundär) | Beide behalten (primär, Autofokus bleibt). |
| Fertig | Banner good, Fuß: Schließen (primär). |
| Fehler | Banner bad mit Fehlertext und ggf. Countdown; Fuß: Schließen | Erneut versuchen (primär, falls angeboten). |

### Versionen-Dialog (`VersionsDialog.tsx`), `modal--lg`

Tabelle: **Version** (Zeilenkopf „Version {n}“ + Badges „Aktuell“ good / „Wiederhergestellt“ info) · **Größe** (`numeric`) · **Gespeichert** (`numeric`) · **SHA-256** (`code`, 12 Zeichen, voller Wert in `title`, `col--optional`) · **Aktionen**: Herunterladen, Wiederherstellen (sekundär sm). Fuß: Schließen (sekundär).

### Mitglieder (`MembersScreen.tsx`)

- Abschnitt „Organisationsmitglieder“: Tabelle **Name** (Zeilenkopf; E-Mail als `.cell__sub` darunter) · **E-Mail** (`col--optional`, für Breite ≥ 1100 px; die Unterzeile bleibt immer) · **Rolle** (Badge neutral) · **Beigetreten** (`numeric`) · **Aktionen**: Rolle ändern (sekundär sm), Entfernen/Verlassen (`danger-quiet` sm).
- Abschnitt „Workspace-Mitglieder“: Abschnittskopf mit Workspace-Name als Eyebrow (`--muted`) und „Zum Workspace hinzufügen“ (sekundär) rechts; gleiche Tabelle.
- Dialoge: Rolle ändern `modal--sm` (Personenname `--ink-strong`, Select Rolle, Abbrechen | Speichern primär); Hinzufügen `modal--md` (Selects Person und Rolle, Ladehinweis, Abbrechen | Hinzufügen primär); Entfernen/Verlassen `modal--sm` mit `danger`.

### Einladungen (`InvitationsScreen.tsx`)

- Abschnitt „Person einladen“: Formular-Raster ab 900 px: Zeile 1 E-Mail (volle Breite bis `--form-max`), Zeile 2 Organisationsrolle | Workspace | Workspace-Rolle (drei Spalten), Zeile 3 Primär „Einladung erstellen“. Schmal: alles untereinander.
- Danach, wenn erzeugt: Kopierfeld ([06](06-components.md#kopierfeld-einmal-link)).
- Abschnitt „Einladungen“: Tabelle **E-Mail** (Zeilenkopf) · **Organisationsrolle** (Badge neutral) · **Workspace** (Name + Rollen-Badge, `col--optional`) · **Status** (Badge: Offen info, Angenommen good, Widerrufen bad, Abgelaufen neutral, Nicht mehr gültig neutral) · **Läuft ab** (`numeric`) · **Aktionen**: Widerrufen (`danger-quiet` sm, nur bei offenen).

### Aktivität (`ActivityScreen.tsx`)

- Datenschutzhinweis als Banner info (kompakt) unter dem Seitenkopf.
- Log-Auswahl (Betreiber) als Select in der Toolbar.
- Abschnitt „Filter“: gerahmte Toolbar (1 px `--line`, 16 px Innenabstand), Raster `repeat(auto-fill, minmax(12rem, 1fr))`, Felder Aktion, Ergebnis, Workspace, Akteur (mono), Von, Bis; Zeile mit Primär „Filter anwenden“ und leise „Filter zurücksetzen“.
- Abschnitt „Ereignisse“: Abschnittskopf mit „Als NDJSON exportieren“ (`<a class="button button--sm">`) rechts. Tabelle **Zeit** (Zeilenkopf, `numeric`; Zusammenfassung „{n}-mal, zuletzt …“ als `.cell__sub`) · **Aktion** (`code`) · **Ergebnis** (Badge: `success` good, `denied` bad, `failed` bad) · **Akteur** · **Ziel** (`code`, `col--optional`) · **Workspace** (`col--optional`) · **Details** (`code`, `col--optional`) · **Request-ID** (`code`, `col--optional`).

### Einstellungen

EmptyState mit dem vorhandenen Text „Diese Ansicht ist noch nicht verfügbar.“.

### System (`SystemScreen.tsx`, nur Betreiber)

- Seitenkopf mit „Aktualisieren“ (sekundär) rechts.
- Gesamtstatus als Banner im passenden Ton (ok → good, degraded → warn, down → bad) mit Badge.
- Banner für Ausfall und Epochenkonflikt wie bisher (bad/warn).
- Abschnitte Komponenten, Uploads, Wartung, Audit-Log, Backups jeweils als Faktenliste; Statuswerte als Badges (`ok` good, `degraded` warn, `down` bad, `not_configured` neutral); Zahlen `tabular-nums`. Ab 1100 px zweispaltiges Raster der Abschnitte (`grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 32px`).
