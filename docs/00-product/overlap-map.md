# Overlap-Landkarte bestehender Cloud-/Account-/Sync-Vorhaben (CC-P00-01)

> **Stand:** 2026-10-28, erstellt mit authentifizierter, rein lesender GitHub-Abfrage (`gh api`, `gh search issues --owner storytold`). Keine Kommentare oder Issues wurden erstellt. Beobachtung (B) ≠ Empfehlung (V).
> **Status:** Entwurf. Muss vor CC-P00-02 von Maintainern bzw. den Autoren der bestehenden Vorhaben bestätigt werden.

## Kernbefund

Die ursprüngliche Recherche (`10-community/source-register.md`) nannte nur drei verwandte Issues. Tatsächlich existieren in der Organisation **zwei bereits weit fortgeschrittene, konkurrierende Vorhaben**, die große Teile des CraftCloud-Umfangs abdecken. Ein neuer, separater CraftCloud-RFC in der vorliegenden Form wäre ein drittes paralleles Vorhaben und widerspräche der eigenen Regel „link existing related issues to prevent duplicate implementations“ (`CONTRIBUTING.md`).

## Vorhaben 1 — CollabCraft (wdunn001)

| Aspekt | Beobachtung (B) |
|---|---|
| Quelle | [`wdunn001/collab-craft`](https://github.com/wdunn001/collab-craft), Rust, Lizenzdateien MIT + Apache-2.0, letzter Push 2026-10-27 |
| Zweck | „Cloud documents and shared editing for the Craft apps. OIDC sign-in, pluggable providers, a live-edit relay, and the browser builds.“ |
| Architektur | Ein Dienst: OIDC-Login (Authentik/Keycloak getestet), Provider-Zugriff (Nextcloud heute), Live-Edit-Relay, Browser-Builds. Redis/Valkey als Bus, **keine eigene Datenbank, schreibt nichts auf Disk**; Dateien liegen beim Cloud-Provider. |
| Org-Integration | Transfer nach `storytold/collab-craft` beantragt in [craft-libs#123](https://github.com/storytold/craft-libs/issues/123) (offen, Repo existiert am Prüftag nicht). |
| Gemeinsame Crates | [craft-libs#160](https://github.com/storytold/craft-libs/pull/160) (`craft-cloud`-Contracts, `craft-suite-account`, Share-/Presence-UI, Open/Save-Pages), [craft-libs#345](https://github.com/storytold/craft-libs/pull/345) (`craft-crdt`) — beide offen, nur Bot-Reviews. |
| App-PRs | Open/Save/Share-PRs u. a. in GridCraft #1087, DeckCraft #711, DesignCraft #878, VectorCraft #2011, WordCraft #1563, FilmCraft #2163, PdfCraft #2765, PhotoCraft #6471, EffectCraft #1623, LightCraft #2060, CadCraft #1576, SoundCraft #774 — laut Autor alle offen. |
| Launcher | [craft-launcher#366](https://github.com/storytold/craft-launcher/pull/366) „Make the launcher the suite's keychain“ — offen. |
| Maintainer-Status | Keine Maintainer-Freigabe sichtbar; keine Org-Repo-Erstellung. |

**Überschneidung mit CraftCloud:** Identität (OIDC statt lokaler Accounts), Open/Save in Apps, Share/Invite, SDK (`craft-cloud`), Launcher-Anmeldung, Live-Collab. **Abweichung:** CollabCraft hat keinen eigenen Datei-/Versionsspeicher, keine eigene Mandanten-DB und kein eigenes RBAC-Schema; diese Aufgaben delegiert es an den Provider (Nextcloud) bzw. OIDC-Gruppen. Genau hier liegt der Kern von CraftCloud (CC-F04–F10).

**Spannungsfelder zu CraftCloud-Prinzipien (zu klären, nicht als Mangel behauptet):** Launcher als Keychain vs. CC-F17 „Launcher ist nicht Auth Authority“; OIDC-Pflicht vs. ADR-0006 „lokale Benutzer zuerst“; Redis-Pflicht vs. `system.md` „keine Pflicht-Redis im MVP“.

## Vorhaben 2 — CraftHub (hasan-sdaif)

| Aspekt | Beobachtung (B) |
|---|---|
| Quelle | Referenzimplementierung [`hasan-sdaif/crafthub`](https://github.com/hasan-sdaif/crafthub); Entscheidungsvorlage [figcraft#357](https://github.com/storytold/figcraft/issues/357) |
| Org-Repo | [`storytold/crafthub`](https://github.com/storytold/crafthub) wurde am 2026-10-15 von Org-Mitglied `tanish2k09` angelegt; Inhalt am Prüftag nur „Placeholder for merges“, keine PRs. |
| Zweck | Accounts/Teams/Rechte über dem bestehenden FigCraft-Kollaborationsprotokoll (v3, abwärtskompatibel zu v2-Links), `craft-collab` als Bibliothek ([craft-libs#128](https://github.com/storytold/craft-libs/pull/128)), Teams, Review, Kommentare, BYO-Cloud-Storage, Marketplace/Payments (optional, separat `craftmarket`). |
| Identität | Konfiguration `auth.mode = off | local | oauth`; laut Autor ([crafthub#1](https://github.com/storytold/crafthub/issues/1)) noch **keine OIDC-/SAML-Implementierung**. |
| Lizenz/Regeln | Autor nennt MIT OR Apache-2.0, clean-room und „craftrules constitution“. |
| Maintainer-Status | Org hat Repo angelegt (strukturelle Zustimmung zum Ort), Code-Merge steht aus. |

**Überschneidung mit CraftCloud:** Organisation/Team/Rollen (CC-F03–F05), FigCraft-Adapter (CC-F16) — CraftHub ist faktisch Variante B aus `04-integrations/figcraft-adapter.md`. Externe Shares (CC-F18), SSO (CC-F19).

## Weitere verwandte Issues (B)

| Issue | Inhalt | Bezug |
|---|---|---|
| [wordcraft#105](https://github.com/storytold/wordcraft/issues/105) | „CraftCloud, decentralized encrypted cloud extension“; 0 Kommentare | Namensquelle; Wunsch „encrypted“ kollidiert mit MVP-Nicht-E2EE |
| [craft-launcher#131](https://github.com/storytold/craft-launcher/issues/131) | Nextcloud-/Open-Source-Cloud-Integration | durch CollabCraft-Nextcloud-Provider adressiert |
| [artcraft#2139](https://github.com/storytold/artcraft/issues/2139) | DriveCraft (Sync & Sharing, BYO-Storage) | DriveCraft ist laut `vision-and-boundaries.md` Nichtziel |
| [artcraft#2230](https://github.com/storytold/artcraft/issues/2230) | Self-hosted ArtCraft cloud, Web-Builds, One-Command-Deployment; verweist auf `artcraft-services/build/selfhost` | Deployment-Überschneidung |
| [filmcraft#350](https://github.com/storytold/filmcraft/issues/350) | Suite-weiter selbst hostbarer Collab-Server | CraftHub/CollabCraft |
| [figcraft#742](https://github.com/storytold/figcraft/issues/742) | Accounts/Teams für Kommentare | CraftHub |
| [crafthub#1](https://github.com/storytold/crafthub/issues/1), [#2](https://github.com/storytold/crafthub/issues/2) | OIDC/SAML; Multi-Account in Apps | CC-F19 |
| [craft-libs#49](https://github.com/storytold/craft-libs/issues/49) | Gemeinsames Places-Panel für Open/Save | CC-F13/P4 |
| gridcraft#243, photocraft#3163, photocraft#4452, vectorcraft#1114, filmcraft#940, effectcraft#706, lightcraft#880, pdfcraft#1000 | Autosave/Versionen auf eigenem Storage (rclone/NAS/S3) | Versionierung ohne Server — alternativer Ansatz |
| gridcraft#888, wordcraft#650, wordcraft#2060, cadcraft#998 | Nextcloud/WebDAV/WOPI | Provider-Ansatz |
| artcraft-services#60 | Suite Manager à la Creative Cloud | Launcher-/Account-Umfeld |

## Abdeckungsmatrix CraftCloud-Feature → bestehendes Vorhaben

| Feature | CollabCraft | CraftHub | Offen / CraftCloud-Alleinstellung |
|---|---|---|---|
| F01 Bootstrap | – (OIDC extern) | unklar | ja |
| F02 Identität/Sessions | OIDC | `auth.mode`, Impl. ausstehend | lokale Accounts |
| F03–F05 Invites/Orgs/RBAC | Invites ja; Rechte via OIDC-Gruppen/Provider | Teams, 8 Rollen × 64 Capabilities (laut Autor) | Tenant-DB-Isolation |
| F06–F10 Dateien/Versionen/Storage/Quota | an Provider (Nextcloud) delegiert | „BYO-cloud storage“ | **eigener immutable Versionsspeicher mit CAS** |
| F11 Audit, F14 Backup, F15 Health | `/healthz`; sonst unklar | unklar | ja |
| F12 Web-Admin | unklar | unklar | ja |
| F13 SDK | `craft-cloud` in craft-libs#160 | `craft-collab` in craft-libs#128 | **zwei konkurrierende Contracts existieren bereits** |
| F16 FigCraft-Adapter | Live-Edit-Relay | Kern des Vorhabens | – |
| F17 Launcher | Keychain-PR #366 | – | – |
| F18 Shares | ja | ja | – |
| F19 SSO | ja (OIDC) | geplant | – |

„unklar“ = am Prüftag nicht verifiziert, nicht „nicht vorhanden“.

## Empfehlung (V) für CC-P00-02

1. **Keinen eigenständigen CraftCloud-RFC** mit neuem Repo-Vorschlag posten, solange zwei Vorhaben auf Maintainer-Entscheidung warten.
2. Stattdessen den RFC umformulieren zu einer **Abstimmungsfrage**: Welches Vorhaben soll Identität/Tenant/Datei-Versionierung besitzen, und fehlt ein provider-unabhängiger, versionierter Dateispeicher mit CAS (CraftClouds Alleinstellung)?
3. Geeigneter Ort: bestehender Thread (craft-libs#123 oder figcraft#357) bzw. eine von Maintainern benannte Stelle — nicht ein viertes Issue.
4. CraftCloud-Briefings (Threat Model, CAS-Commit-Protokoll, Backup/Restore, Negativtest-Matrix) als **Review-Input** für das gewählte Vorhaben anbieten.

Entscheidung liegt beim Nutzer bzw. den Maintainern; diese Datei trifft sie nicht.
