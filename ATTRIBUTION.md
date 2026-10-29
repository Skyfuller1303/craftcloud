# Asset attribution / provenance

This documentation-first blueprint bundles **no images, logos, icons, fonts, or other art assets**. There are therefore no third-party entries yet. In a real implementation, document each distributed asset path, creator, original source URL, exact license and modifications; do not copy brand assets without permission.

## Design token values (admin UI)

The admin UI (`apps/craftcloud-admin`) distributes no images, logos, icons or fonts; it uses system fonts. The following numeric values in `apps/craftcloud-admin/src/styles/_tokens.scss` were copied from the Craft launcher theme (ADR-0015):

| Values | Source | License |
| --- | --- | --- |
| Colour tokens `bg`, `sunken`, `ink`, `ink-strong`, `accent`, `accent-ink`, `invert-bg`, `invert-fg`, `good`, `warn`, `bad` and the alpha steps of `muted`, `faint`, `line`, `line-strong` (light and dark); control height 34 px; label size 11 px with 0.15 em tracking | `crates/ui-egui/src/theme.rs` (`Tokens::DARK`, `Tokens::LIGHT`, `CONTROL_H`, `HUD_SIZE`) in `storytold/craft-launcher`, local checkout at commit `7839c99924e7fc2854c6ed805f66f84332ffb83f` | MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE` of that repository) |

Not copied: fonts (the launcher bundles Inter, Archivo and Geist Mono; CraftCloud does not), logos, icons, images. The token `--control-border`, the spacing scale and all other values are original to CraftCloud. The alpha values were derived from the launcher's 0-255 alpha channels (for example 153/255 = 60 %).
