# CraftCloud egui demo

The demo connects to a CraftCloud server and uses the shared `craftcloud-egui` dialogs. It has no bundled account, token, or sample credentials. Use synthetic data on a local development server.

Run from the repository root using the bounded Cargo launcher:

```powershell
$env:CRAFTCLOUD_DEMO_SERVER = 'http://127.0.0.1:18084'
$env:CRAFTCLOUD_DEMO_DATA_DIR = 'D:\ArtCraft\output\craftcloud-codex\AC-04\demo-data'
$env:CARGO_HOME = 'D:\ArtCraft\.tools\rust\cargo'
$env:RUSTUP_HOME = 'D:\ArtCraft\.tools\rust\rustup'
$env:CARGO_BUILD_JOBS = '2'
python -I D:\ArtCraft\output\figcraft-pr634-review\run-memory-limited.py --gib 8 --json-log D:\ArtCraft\output\craftcloud-codex\AC-04\demo.json -- D:\ArtCraft\.tools\rust\cargo\bin\cargo.exe run -p craftcloud-egui-demo
```

Replace the example port with the port of your own disposable instance; the project owner's port 8080 is not used by the AC-04 acceptance workflow. Use the server URL, email, and password in the UI. For screenshot automation, set `CRAFTCLOUD_DEMO_EMAIL` and `CRAFTCLOUD_DEMO_PASSWORD`; the demo signs in after discovery. These environment values are read only by the demo process and never logged. The demo stores refresh tokens only through the operating system keyring. Use a disposable local account if the host has no keyring service; the session then ends when the application closes. `CRAFTCLOUD_DEMO_DATA_DIR` redirects cache and link files for isolated screenshot runs.

The first screen supports server discovery and device login. After sign-in, browse organizations, workspaces, projects, files, and shared items; download a selected version; or create a file and upload a selected local sample. Conflict actions create a copy or fetch the current head before a confirmed overwrite. Lock actions open read-only or cancel; closing the cloud file releases its lease. Use two synthetic accounts to exercise conflict and lock cases. The current server exposes `SECOND_FACTOR_REQUIRED` but no AC-02 completion route, so the dialog accepts a code but cannot complete that login yet. Cloud operations begin only after a direct user action.
