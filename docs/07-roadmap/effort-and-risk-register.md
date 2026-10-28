# Aufwand, Unsicherheit und Risikoregister

## Kapazität (grobe Planungsbandbreiten, keine Zeitzusage)

Die folgenden Schätzungen sind nur relative Priorisierung für ein erfahrenes Team mit CI, Security-Review und vorhandenen Rust-/React-Kenntnissen. Sie sind **keine Kalender- oder Lieferzusage**. Integration/Abnahme mit externen Maintainern und Produktionssecurity können die Zahlen erheblich verändern.

| Phase | Grobe Engineering-Tage | Unsicherheit |
|---|---:|---|
| P0 Konzept/Abstimmung | 5–15 | hoch durch Maintainer-Feedback |
| P1 API/Auth/RBAC | 20–45 | hoch durch Auth-/Security-Gates |
| P2 Storage/Versioning | 25–55 | sehr hoch durch Durability/Edge Cases |
| P3 Admin/Operations | 20–45 | hoch durch Restore-/Security-Gates |
| P4 SDK/eine App | 12–35 | sehr hoch durch App-Innenarchitektur |
| P5 FigCraft/Launcher | 25–70 | sehr hoch durch Cross-Repo-Protocol |
| P6 Enterprise | 30–90 | sehr hoch durch SSO/S3/E2EE-Fragen |
| P7 Pilot/RC | 10–30 | externe Befunde |

Die Summe ist nicht eine seriöse Sprintdauer: PR-Reviews, Entscheidungszeit, Parallelisierung, Testinfrastruktur und notwendige Rework-Zeit fehlen. Erst nach zwei implementierten Vertikalschnitten neu kalibrieren.

## Risikoregister

| ID | Risiko | Wahrscheinlichkeit/Impact | Mitigation |
|---|---|---|---|
| R01 | Repo/Lizenz/Name nicht gewünscht | hoch/mittel | P0 RFC, eigener neutraler Prototyp |
| R02 | `craftrules` Regeln fehlen | hoch/mittel | Zugriff vor Compliance-Behauptung |
| R03 | Cross-tenant Access leak | mittel/sehr hoch | negative auth tests, policy, DB scoping |
| R04 | DB und Blobstore inkonsistent | hoch/sehr hoch | pending/verified/commit, restore drills |
| R05 | FigCraft Protokoll divergiert | hoch/hoch | adapter RFC, no duplication |
| R06 | Native Auth UX verschlechtert offline | mittel/hoch | explicit opt-in and offline tests |
| R07 | Docker Selbsthosting fehleranfällig | mittel/hoch | minimal dependencies, installer tests |
| R08 | Unklarer Datenschutz/E2EE Claim | mittel/hoch | clear language, no false encryption claim |
| R09 | API Breaking Changes | hoch/mittel | OpenAPI diff, version negotiation |
| R10 | Security maintenance burden | mittel/sehr hoch | narrow MVP, dependency policy, incident response |

## Go/No-go Entscheidung

P0 No-go bei ungeklärter Lizenz für kopierten Code oder unerlaubter Markenverwendung; P1 No-go bei ACL-Lücken; P2 No-go bei Datenverlust nach Crash; P3 No-go ohne getestete Backups; P4 No-go wenn Cloudpflicht für lokale App entsteht; P5 No-go ohne FigCraft-Maintainer-Akzeptanz.
