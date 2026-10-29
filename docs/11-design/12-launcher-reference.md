# 12 — Launcher-Referenz

Vorbild ist der Craft-Launcher `storytold/craft-launcher`, lokale Kopie `D:\ArtCraft\craft-launcher`, Commit `7839c99924e7fc2854c6ed805f66f84332ffb83f`, Verzeichnis `crates/ui-egui/src/`. Der Launcher ist eine egui-Desktop-App; wir übernehmen **Maße, Farbwerte und Regeln**, keinen Code, keine Schriften, keine Bilder.

Die Kopie darf zum Nachmessen gelesen werden. Nichts daraus wird in dieses Repository kopiert außer Zahlenwerten, die in `apps/craftcloud-admin/ATTRIBUTION.md` mit Datei, Zeile, Commit und Lizenz vermerkt sind.

## Fundstellen

| Thema | Launcher-Stelle | Übernommen in |
|---|---|---|
| Farbpalette hell/dunkel | `theme.rs:80-117` | [03](03-colors.md) |
| Akzent/Statusfarben | `theme.rs:134-137` | [03](03-colors.md) |
| Schriftgrößen Body/Mono | `theme.rs:201-208` | [04](04-typography.md) |
| HUD-Label (Großschreibung im Zeichnen, Original bleibt) | `widgets.rs:4,26` | [04](04-typography.md), [11](11-content-and-i18n.md) |
| HUD-Stil (11 px, 0.15 em) | `widgets.rs:29-52` | [04](04-typography.md) |
| Tag (mono 11/700, 4×8, Vollfläche) | `widgets.rs:74-81` | [02](02-brand-and-logos.md), [06](06-components.md) |
| Button-Laufweite 0.12 em | `widgets.rs:84-88` | [04](04-typography.md), [06](06-components.md) |
| Badge (20 px, 0 6 px, mono 10) | `widgets.rs:282-290` | [06](06-components.md) |
| Buttons Varianten/Hover | `widgets.rs:310-329`, `323-325` | [06](06-components.md) |
| Felder / Auswahl | `widgets.rs:341-395` | [06](06-components.md) |
| Fortschritt (4 px, 30 %-Segment) | `widgets.rs:398-425` | [06](06-components.md), [09](09-motion-and-feedback.md) |
| Selbst gezeichnete Glyphen | `widgets.rs:439-489`, `447-473` | [07](07-icons.md) |
| Tabellenansicht | `views.rs:477-632` | [06](06-components.md), [08](08-screens.md) |
| Leerzustand | `views.rs:138-139` | [06](06-components.md) |
| Dialog Maße | `dialogs.rs:22-23`, `71-87` | [06](06-components.md) |
| Dialog-Fuß: Abbrechen links, Bestätigen rechts | `dialogs.rs:168-177` | [06](06-components.md), [10](10-accessibility.md) |
| Bewegung | `motion.rs:4` | [09](09-motion-and-feedback.md) |
| Kopfleiste | `app.rs:616-619` | [05](05-spacing-and-layout.md) |
| Toast-Dauer, max. Anzahl | `app.rs:258`, `259` | [09](09-motion-and-feedback.md) |
| Toast-Position/Stapel | `app.rs:728-763` | [06](06-components.md), [09](09-motion-and-feedback.md) |

## Bewusste Abweichungen vom Launcher

| Launcher | Admin-UI | Grund |
|---|---|---|
| Inter / Archivo / Geist Mono | Systemschriften | keine Craft-Schriften (ADR-0015) |
| Fortschritt läuft bei reduzierter Bewegung weiter | angehalten | WCAG 2.3.3 |
| Großschreibung beim Zeichnen | `text-transform` | Screenreader lesen Original |
| Kontraste teils < 4,5:1 (`--faint`) | `--faint` nie für Text | WCAG 1.4.3 |
| Hover ~83 ms / 300 ms | einheitlich 120 ms | Einfachheit |
| Toasts sind Hauptkanal | Toasts `aria-hidden`, Live-Region bleibt | keine Doppelansage |
| Zeilenmenü ⋮ | sichtbare Zeilenaktionen | Funktionsänderung, nicht Teil dieses Durchgangs |

Neue Abweichungen im PR begründen.
