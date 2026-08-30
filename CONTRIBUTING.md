# Contributing

## Prerequisites (Windows)

- Rust stable (≥ 1.75) — install via [rustup.rs](https://rustup.rs/).
- Microsoft Edge WebView2 — preinstalled on Windows 10 ≥ 1809 and Windows 11.
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`.
- (Runtime only) GPU with hardware HEVC encoder support — NVIDIA, AMD, or Intel iGPU. The crate **builds** without one; only running the engine end-to-end needs it.

For the Android client (Kotlin): Android Studio Hedgehog (2023.1.1) or newer, with platform 34 + NDK r26b.

## Build the workspace

```powershell
git clone <repo-url> zpenflow
cd zpenflow
cargo build --workspace
```

## Run the GUI

```powershell
cd apps\penflow-gui\src-tauri
cargo tauri dev
```

A window titled "Penflow" opens. Engine wiring is in progress — see [`docs/design.md`](docs/design.md) for the plan.

## Testing a local build against an existing install

If Penflow is also **installed** (`C:\Program Files\Penflow\penflow-gui.exe`),
double-clicking your freshly built `target\release\penflow-gui.exe` does not
run your build. With `run_as_admin: true` in
`%APPDATA%\Penflow\settings.json`, `main.rs` hands off to the scheduled task
`Penflow` — which is registered to the **installed** exe — and then exits. The
installed release keeps running and your fix appears to do nothing.

Launch from an elevated shell instead; `is_elevated()` is already true, so
there is no handoff:

```powershell
# quit the running instance first (tray -> Quit; that also disables the VDD)
& 'C:\path\to\zpenflow\target\release\penflow-gui.exe'
```

Verify which binary is live before trusting a measurement — a running exe is
file-locked, and `Win32_Process.ExecutablePath` comes back empty for an
elevated process queried from an unelevated shell:

```powershell
$p = 'C:\path\to\zpenflow\target\release\penflow-gui.exe'
try { [IO.File]::Open($p,'Open','Write','None').Close(); 'not running' }
catch { 'this build IS running' }
```

## Tests + lints (must pass before pushing)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

GitHub Actions runs the same four commands on `windows-latest` for every push and PR.

## Hardware-dependent integration tests

Tests that require a live GPU + desktop session are marked `#[ignore]`. Run them locally before opening a PR that touches capture/encode:

```powershell
cargo test -p penflow-core -- --ignored
```

## Project conventions

- `cargo fmt --all` is the formatter. No exceptions.
- `cargo clippy -- -D warnings` is the lint bar. No new warnings, ever.
- Each commit is a working tree on its own (CI passes).
- Public APIs documented; `#![deny(missing_docs)]` enforced on crate roots where appropriate.
- See [`docs/design.md`](docs/design.md) §15 for known risks and §17 for open questions.
