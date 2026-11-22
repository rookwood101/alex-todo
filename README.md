# Rustboard

**Rustboard** is the first milestone of the **alex-todo** project: a dark, infinitely pannable
canvas for sticky notes, built as a native Windows app with Rust and
[`egui`/`eframe`](https://github.com/emilk/egui) on WGPU.

![Rustboard running on Windows](docs/screenshot.png)

## Features

- Infinite, zoomable canvas with an adaptive grid
- Dark UI with pastel sticky notes
- Inline multiline text editing on every note
- Drag a note's header to move it; dragged notes move with the pointer and come to the front
- Wheel scrolling zooms directly — no Ctrl needed — anchored under the cursor
- Hold the right **or** middle mouse button and drag to pan in 2D, including over notes
- Add, select, delete, and fit-to-content controls
- Every edit is saved automatically to a local state file and restored on next launch, with a visible
  save status and `Ctrl+S` to save on demand
- Low-latency input path: raw pointer updates are applied before painting, active gestures request
  repaints, and the WGPU surface uses one queued frame with a no-vsync presentation mode

## Controls

| Input | Action |
|---|---|
| Scroll wheel | Zoom in/out under the pointer |
| Right-drag or middle-drag | Pan the canvas |
| Drag a note header | Move the note |
| Click note text | Edit the note |
| `Ctrl+N` | Add a note at the center of the view |
| `Ctrl+S` | Save immediately (also works while typing in a note) |
| `Delete` / `Backspace` | Delete the selected note when not typing |
| `+ New note` | Add a note and focus its editor |
| `Delete` button | Delete the selected note, even while editing |
| `Fit view` | Frame every note on the board |

Notes are draggable only by their header; primary dragging inside the text body is reserved for
text selection and caret placement.

## Build and run

Requirements:

- Windows 10/11
- Rust stable 1.95 or newer (tested with 1.100.0)
- MSVC C++ build tools

```powershell
cargo run --release
```

If `cargo` is not on your PATH, use the installed toolchain directly:

```powershell
$rustBin = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin"
$env:PATH = "$rustBin;$env:PATH"
& "$rustBin\cargo.exe" run --release
```

For a release build that is also code-signed in one step:

```powershell
.\scripts\build.ps1
```

The script finds `cargo` even when it is not on `PATH`, refuses to run while the app is open, and
fails if the signature does not verify.

### Code signing (local development)

Unsigned builds are treated as an unknown publisher by UAC and SmartScreen, so `scripts\build.ps1`
signs every release build with a local development certificate. Create and trust that certificate
once:

```powershell
.\scripts\build.ps1 -CreateCertificate
```

The certificate is created in `Cert:\CurrentUser\My`, trusted in the current user's `Root` and
`TrustedPublisher` stores, and its public half is kept at
`%USERPROFILE%\.rustboard\signing\rustboard-dev.cer`. To sign a build by hand instead:

```powershell
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -like 'CN=Rustboard Development*' }
Set-AuthenticodeSignature -FilePath .\target\release\rustboard.exe -Certificate $cert
```

The signature is attached to the executable, so it must be re-applied after every rebuild, and the
executable must not be running while it is signed. A self-signed certificate is only trusted where
`rustboard-dev.cer` has been imported; sharing the app with other people needs a real code-signing
certificate.

## Tests

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The suite currently has 28 tests:

- Pure model tests for seed validity, add/delete, z-ordering, bounds, RON persistence, and
  malformed-state repair
- Camera tests for coordinate round-tripping, zoom anchoring, clamping, pan direction, fit-to-view,
  and adaptive grid spacing
- State-file tests for atomic replacement, corrupt-file reporting, load priority, and falling back to
  the next location when a folder is not writable
- App tests for dirty tracking, importing an older eframe payload, purging the old storage key, and
  reporting failed saves
- Headless egui interaction tests that run without opening a window. They synthesize pointer,
  wheel, keyboard, and text events through `egui::Context` and verify:
  - wheel zoom over note text keeps the point under the cursor fixed
  - right-drag and middle-drag panning work over notes
  - dragging a note header updates its position before the button is released
  - clicking note text focuses the editor and typing reaches the note
  - typing in a note writes it to the state file
  - `Ctrl+N` adds a note

## Persistence

Notes and the camera are written as RON to a `board.ron` state file. Its location is the first usable
candidate:

1. `RUSTBOARD_STATE_PATH`, when set (useful for pinning a board to a folder)
2. `%APPDATA%\rustboard\data\board.ron`
3. `board.ron` next to the executable, when the app-data folder cannot be created
4. `board.ron` in the working directory

Writes are atomic (scratch file, then rename) and happen after every edit — coalesced to at most one
write per 250 ms — plus on window close and on `Ctrl+S`. The toolbar shows `Saving…`, `Saved`, or
`Save failed`; hovering it reveals the exact file in use and clicking it forces a save. State that
cannot be parsed is reported in the toolbar and the sample board opens instead of crashing. Boards
saved by earlier versions under the eframe key `rustboard.state.v1` are imported once, then that key
is removed.

## Project layout

| Path | Contents |
|---|---|
| `src/app.rs` | App state, action reducer, toolbar, hint bar, shortcuts, persistence |
| `src/canvas.rs` | Custom camera-driven canvas, grid, notes, text editors, pointer input |
| `src/board.rs` | Serializable note/board model and repair rules |
| `src/camera.rs` | Camera transforms, zoom anchoring, fit-to-view, grid spacing |
| `src/persist.rs` | State-file location resolution, atomic save/load, failure reporting |
| `src/theme.rs` | Dark palette and note colors |
| `src/headless_tests.rs` | Window-free egui interaction tests |
| `docs/screenshot.png` | Current UI reference |
| `scripts/build.ps1` | Release build and code-signing helper |

## Current scope

This iteration is a single local board. Notes have a fixed size; there is no note resizing,
undo/redo, collaboration, accounts, sync, or AI/todo functionality yet.

## Direction

The repository is named `alex-todo` because this visual workspace is intended to grow into a
personal todo system. The current release deliberately focuses on the foundation: a fast, pleasant
infinite canvas with editable sticky notes.

## License

MIT — see [LICENSE](LICENSE).
