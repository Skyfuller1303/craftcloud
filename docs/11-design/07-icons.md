# 07 — Icons

## Grundsatz

Der Launcher zeichnet seine UI-Glyphen selbst (Pause, Schließen, ⋮, Sortierdreieck, Statusquadrat; `widgets.rs:439-489`) und nutzt keine Icon-Schrift. Wir machen es genauso: wenige, einfache, selbst gezeichnete SVG-Formen. Buttons sind grundsätzlich **reiner Text**; Icons nur dort, wo sie Bedeutung tragen (Schließen, Chevron, Haken, Kopieren, extern, Status).

Verboten: Icon-Fonts, Icon-Pakete (auch kostenlose), Emoji, kopierte SVGs aus dem Launcher oder von getartcraft.com, Social-Icons.

## Komponente

`src/components/Icon.tsx`:

```tsx
type IconName = "chevron-down" | "close" | "check" | "external" | "copy" | "status";
export function Icon({ name, size = 16 }: { name: IconName; size?: 12 | 16 }) {
  return (
    <svg className={`icon icon--${name}`} width={size} height={size} viewBox="0 0 16 16"
         fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="square"
         aria-hidden="true" focusable="false">
      {PATHS[name]}
    </svg>
  );
}
```

- Farbe immer `currentColor` (folgt dem Text des Elements).
- Keine `style`-Props (CSP). Größe über `width`/`height`-Attribute.
- `aria-hidden="true"`: Bedeutung trägt der zugängliche Name des Elternelements.

## Satz

Koordinaten im 16 × 16-Raster, Linienstärke 1.5, Enden eckig.

| Name | Pfade | Verwendung |
|---|---|---|
| `chevron-down` | `<path d="M4 6l4 4 4-4" />` | Select-Auslöser |
| `close` | `<path d="M3.5 3.5l9 9M12.5 3.5l-9 9" />` (Launcher: halbe Größe 4.5) | Dialog/Banner/Toast schließen |
| `check` | `<path d="M3 8.5l3 3 7-7" />` | ausgewählter Listeneintrag, Checkbox, Kopiert-Bestätigung |
| `external` | `<path d="M6 3h7v7M13 3L5 11" />` | externe Links (alternativ Textzeichen `↗`) |
| `copy` | `<path d="M5.5 5.5h7v7h-7z" /><path d="M3.5 10.5v-7h7" />` | Kopieren-Icon-Button |
| `status` | `<rect x="5" y="5" width="6" height="6" fill="currentColor" stroke="none" />` | Statusquadrat (Banner, Toast, Feldfehler, Schritte) |

Weitere Icons nur, wenn eine Funktion ohne sie unverständlich wäre; dann im selben Stil ergänzen und hier dokumentieren.

## Größen

- 16 px Standard (Selects, Icon-Buttons).
- 12 px in Checkboxen und Badges.
- Abstand Icon↔Text 8 px.
