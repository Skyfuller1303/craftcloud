# ADR-0008: Lizenz- und Markenfreigabe vor Repository-Veröffentlichung

- **Status:** Entschieden für unabhängig entwickelten CraftCloud-Code; Marke und Rechte Dritter separat offen
- **Datum:** 2026-11-02
- **Owner:** noch zu benennen

## Kontext

`storytold` Craft-Apps sind vielfach MIT/Apache-2.0, artcraft-services dagegen Fair-Source/WIP. CraftCloud Name/Logos müssen geklärt sein.

## Frühere Empfehlung (2026-10-28)

Clean-room Vorschlag ohne proprietären Code/Assets; MIT OR Apache-2.0 nach Rechteinhaber-/Maintainer-Freigabe.

## Entscheidung (2026-11-02)

Unabhängig entwickelter CraftCloud-Code wird unter MIT OR Apache-2.0 dual-lizenziert. Nutzer wählen eine der beiden Lizenzen. Die Lizenzierung beansprucht kein Eigentum an CraftCloud oder ArtCraft-Marken und erteilt keine Berechtigung zur Nutzung von Namen, Logos, Marken oder Assets Dritter. Repository-Zugehörigkeit, Branding und etwaige Rechte an übernommenen Drittinhalten bleiben getrennte Fragen. Eingeschränkt lizenzierter Code aus `artcraft-services` bleibt ausgeschlossen.

## Konsequenzen und Risiken

Lizenzdateien und Paketmetadaten deklarieren MIT OR Apache-2.0. Branding, Rechteinhaberschaft und Fremdinhalte erfordern weiterhin eigene Klärung.

## Betrachtete Alternativen

Unkommentiertes Kopieren eines Lizenzheaders oder Behauptung der Markenberechtigung.

## Review und Revision

Diese Entscheidung gilt für unabhängig entwickelten CraftCloud-Code. Änderungen daran erhalten eine neue ADR statt stiller Umschreibung.
