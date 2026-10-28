# Reviewfragen für `storytold` Maintainer

## Repository/Branding

- Gibt es bereits ein privates/intensiv geplantes Cloud/Collaboration-Projekt?
- Gehört dieses Vorhaben organisatorisch zu ArtCraft, zu Craft Apps oder zur Community?
- Darf der Arbeitstitel `CraftCloud` genutzt werden? Welche Namenskonvention für Crates/Binaries und sichtbaren Produktnamen?
- Welche Lizenz, NOTICE- und Attribution-Vorgaben? Wer entscheidet über Marks und Logos?
- Wo ist `craftrules` verfügbar, und welche Version gilt? Welche Pflicht-Gates sollen wir importieren?

## Architektur/API

- Wo soll das versionierte Client SDK liegen (`craft-libs` oder eigene Repo)?
- Sollen Online-Accounts mit Hosted ArtCraft verknüpft werden? Falls ja, wie ohne Cloud-Pflicht und ohne Source-Lizenzkonflikt?
- Ist SQLite oder PostgreSQL bevorzugt? Sollen rein lokale Installationen und Firmen-Deployments dieselbe DB verwenden?
- Wer besitzt die Definitionshoheit über User, Project, Workspace und File ID?
- Welche App eignet sich als erster synthetischer Integrationstest?
- Wie wird im FigCraft-Server der Übergang von Share-Token auf personalisierte Rollen betrachtet?

## Prozess

- Issue/Discussion/Projektboard als RFC-Kanal?
- Branch policy, CI, release, code ownership, signing, security reporting?
- Welche Anforderungen sind für ersten Prototyp und erste externe Nutzer unverzichtbar?

**Antworten** bitte in `docs/00-product/open-questions.md` und relevante ADRs übertragen, keine impliziten Entscheidungen in Code.
