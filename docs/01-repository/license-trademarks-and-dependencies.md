# Lizenz, Marken und Abhängigkeiten — Pflichtprüfung

## Beobachtete Lizenzen

- Craft Launcher, FigCraft, craft-libs, craft-ai-integrations: öffentlich dual MIT/Apache-2.0 ausgewiesen.
- `artcraft-services`: spezielle `ArtCraft License (WIP)` mit Verwertungsbeschränkungen; **kein** unverändert übernehmbares MIT/Apache-Backend.

## Vorgeschlagene Regel

Für neue, selbst geschriebene CraftCloud-Bestandteile könnte MIT OR Apache-2.0 passen, wenn Rechteinhaber und Maintainer zustimmen. Dieses Paket verteilt bewusst keine unautorisierte LICENSE-Datei. Übernahme von Snippets, APIs, Designs, Logos, Fonts, Datenbank-/Auth-Code oder Marken muss einzeln geprüft werden. Interfaces können nach üblichen Interoperabilitätsgrundsätzen eigenständig beschrieben werden, aber rechtliche Prüfung ersetzt dies nicht.

## Dependencies

Vor Aufnahme: Lizenz kompatibel? Aktiv gewartet? Security advisories? Minimale Feature-Flags? Transitive Abhängigkeiten? TLS-Korrektheit? Backups? Fehlermanagement? Reproduzierbares Lockfile? Keine GPL-/AGPL-Copy-Paste-Bausteine bei strikter Vereinbarung mit FigCraft-Regeln ohne Maintainer-Freigabe.

## Marken/Assets

`CraftCloud` ist Arbeitstitel. Keine ArtCraft-/Craft-App-Icons mitliefern. Keine Adobe-/Figma- oder anderen Drittanbieterdesigns kopieren. Ursprünglich erstellte Grafiken und zulässige Assets in einem echten Repo mit `ATTRIBUTION.md` dokumentieren. Externe Fonts nur nach Lizenzprüfung, kein Binärfont in diesem Blueprint.

## Exit-Kriterien

- Gewünschte Repo/Trademark-Nutzung freigegeben.
- Lizenzdateien inklusive vollständigem offiziellen Text durch Berechtigte hinzugefügt.
- Dependency Inventory, SPDX-Review und Third-Party Notices vorbereitet.
- Security/Kontaktverantwortliche benannt.
