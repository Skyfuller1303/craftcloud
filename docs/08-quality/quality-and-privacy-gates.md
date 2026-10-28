# Quality gates, accessibility, documentation and privacy

## Build & lint

- Rust compiler/channel aligned with target Craft conventions at implementation date (currently some sibling repos use Rust 1.95). No invented pinned toolchain before builders/maintainers review.
- `cargo fmt`, Clippy deny warnings/panics/unsafe, unit/integration tests; typed config; docs link checker; license/dependency scanner.
- Frontend TypeScript strict, no unchecked `any` around auth payloads, keyboard focus navigation, reduced-motion support.

## Performance & limits

Document test environment (CPU, RAM, network, database, file system) and size/profile, not generalized speed claims. Capture memory peak during 1GB streaming to verify boundedness. Fixed quotas and request limits should prevent slowloris-like resource exhaustion.

## Privacy

Data inventory for account emails, IPs, file metadata, file content, audit, token hashes and backups. Define owner/controller roles according to deployment, retention, logging defaults, export/deletion. No external telemetry or automatic upload without opt-in. TLS protects transit; server operator can read unencrypted file storage unless separate encryption design exists.

## Accessibility

Admin UI WCAG 2.2 AA target: tab order, visible focus, screenreader labels, dialogs, role/group semantics, errors on fields, contrast, reduce motion, non-color-only status. Validate using automated checks **and** manual keyboard/screenreader tests. Never mark AA compliance solely on automated scan.

## Release blockers

Critical auth/tenant/security gap, unreliable restores, false E2EE claims, unscoped data accesses, incompatible update with silent data loss, mandatory Launcher login, unlicensed assets or restricted code reuse. Legal/privacy audit before commercial offering.
