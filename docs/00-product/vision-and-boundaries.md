# Produktvision, Zielgruppen, Abgrenzung

## Ziel

CraftCloud ist eine **optionale, selbst hostbare** und später optional managed angebotene Plattform, die mehreren Craft-Apps gemeinsame **Identitäten, Organisationen, Projekte, Dateien, Versionen und Berechtigungen** bietet. Es ist ein Dienst und keine Voraussetzung für lokales Arbeiten. Der Launcher darf weiterhin direkt in der App-Übersicht starten.

## Primäre Nutzerreisen

1. **Privat, rein lokal**: App starten, Projekt lokal öffnen, ohne Server und Konto arbeiten; die App darf keine Cloud-Abhängigkeit erzwingen.
2. **Kleines Studio (2–10 Personen)**: Administrator installiert CraftCloud auf eigenem Server, legt eine Organisation und einen Workspace an, lädt Kolleginnen und Kollegen ein und verwaltet projektbezogene Freigaben.
3. **Unternehmen mit IT**: SSO später zuschalten, Teams zentral verwalten, Protokolle exportieren, Speicher und Backup kontrollieren; Datensouveränität bleibt beim Betreiber.
4. **Externer Gast**: projektbeschränkten Zugang mit befristeter Freigabe bekommen und ohne Einsicht in andere Workspaces arbeiten.
5. **App-Entwickler**: Eine lokale Datei laden, über Cloud-Client mit `ETag` hochladen, Konflikte auflösen, ohne Benutzerverwaltung selbst nachzubauen.

## MVP-Kernpfad

`Server installieren → initialen Admin sicher erzeugen → Organisation/Workspace erstellen → Mitglied einladen → Projekt anlegen → Datei hochladen → Version speichern → anderen Nutzer lesend zugreifen lassen → Backup und Restore überprüfen`.

## Nichtziele bis nach MVP

- Keine vollständige DriveCraft-Alternative mit Desktop-Filesystem-Treiber.
- Keine globale Accountpflicht, keine zentralen AI-Credits, kein Modellhosting, keine Billing-Pflicht.
- Keine Semantik, die `.figcraft`, `.wordcraft` usw. serverseitig verstehen oder rendern muss; zunächst opake Dateien.
- Kein CRDT-/OT-Neubau; Echtzeit-Kollaboration bleibt zunächst beim jeweiligen App-Server.
- Keine Behauptung von Zero-Knowledge/E2EE bei serverseitiger Dateiverwaltung.
- Keine transparente Nextcloud-, WebDAV- oder Google-Drive-Kompatibilität ohne eigene Protokoll- und Sicherheitsprüfung.

## Produktprinzipien

Local-first, opt-in, self-hostable, least privilege, recoverable, interoperable via versioned contract, explicit user consent for uploads, minimal baseline dependencies, and accessibility. Keine Werbung für einen offiziell bestehenden `storytold/craftcloud`-Dienst, solange keiner existiert.

## Erfolgskriterien

Die MVP-Demo muss die Kernreise auf frisch installiertem Docker-Setup mit zwei Accounts und zwei Workspaces wiederholbar erfüllen. Eine nicht autorisierte Person sieht weder Metadaten noch Dateiinhalt eines fremden Workspaces. Wiederherstellung aus einem konsistenten Backup wurde praktisch getestet. Ein ausgelöstes Netzwerkproblem lässt lokale Craft-Dateien unbeschädigt.
