# ADR-0005: Separate React-Adminoberfläche trotz Rust-only Launcher

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

Browser-Admin für Self-hosting hat andere Plattformanforderungen als Craft Launcher. `artcraft-services` zeigt React als Web-Frontend, Launcher ist in seinem Repo ausdrücklich Rust-only.

## Entscheidungsvorschlag

React/TS als separates `apps/craftcloud-admin`; Launcher-Änderungen später ausschließlich nach dessen Rust/egui-Regeln.

## Konsequenzen und Risiken

Gemischter Monorepo mit JS toolchain, aber keine Verletzung der Launcher-Regeln.

## Betrachtete Alternativen

egui/WASM-Admin oder serverseitiges HTML; denkbar, bei Maintainer-Wunsch neu entscheiden.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
