# Asset and value attribution

## Original artwork

`public/favicon.svg` is an original geometric mark created for CraftCloud. No
third-party image, logo, font, icon, or application asset is included.

## Numeric values adapted from Craft Launcher

The following values were read from `storytold/craft-launcher`, commit
`7839c99924e7fc2854c6ed805f66f84332ffb83f`, under its MIT OR Apache-2.0
license. Only numeric values and general UI principles were used; no source
code, glyph paths, fonts, or artwork were copied.

| Values used here | Source |
|---|---|
| Light/dark surface, text, line, accent, status, inverse, and shadow palette | `crates/ui-egui/src/theme.rs:80-117`, `theme.rs:134-137` |
| System replacement sizes based on launcher body, small, and HUD sizes: 14 px, 12.5 px, 11 px; HUD tracking 0.15 em | `crates/ui-egui/src/theme.rs:201-208`, `widgets.rs:29-52` |
| Button tracking 0.12 em and 8 px icon/text spacing | `widgets.rs:84-88` |
| Wordmark tag: 11 px, weight 700, 4 px vertical and 8 px horizontal padding, 0.15 em tracking | `widgets.rs:74-81` |
| Badge: 20 px minimum height, 6 px horizontal padding, 10 px monospaced text | `widgets.rs:282-290` |
| 34 px controls, secondary/primary hover treatment, field and selection states | `widgets.rs:310-395` |
| 4 px progress track and 30% indeterminate segment | `widgets.rs:398-425` |
| Table row and header treatment; empty-state 40 px top padding | `views.rs:477-632`, `app.rs:616-619` |
| Dialog 24 px interior spacing and cancel-before-confirm order | `dialogs.rs:22-23`, `dialogs.rs:168-177` |
| Four visible notifications, 4.5 s normal lifetime, 20 px screen edge, 380 px maximum width, 6 px stack gap, 10 px × 14 px padding | `app.rs:258-259`, `app.rs:728-763` |

## Original CraftCloud values

These values were selected for this interface, accessibility, or responsive
layout and were not taken from the launcher:

- `--control-border`, `--danger-fg`, `--scrim`, tint strength, spacing scale,
  48 px table rows, 208 px navigation, 560 px forms, and 400/480 px auth cards.
- `--focus-ink` provides a 3:1 focus indicator on both light and inverted
  surfaces; the launcher accent ink falls below 3:1 on the light inverted
  surface.
- `--progress-track` and `--progress-fill` provide a 3:1 progress indicator
  contrast; the launcher blue accent against the launcher hairline does not.
- 120 ms motion duration is an original CraftCloud value, chosen as a single
  transition duration for this interface.
