# wispr-flow-linux helper - Development Notes

This is the clean-room Rust helper for the unofficial Wispr Flow for Linux
port: a standalone process the Electron app spawns to inject text, report the
active app, and feed it global key events.

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## Required reading

- [`ipc-contract.md`](https://github.com/wispr-flow-linux/wispr-flow-linux/blob/main/docs/reference/ipc-contract.md)
  (+ `keycodes.json`, `commands.json`) in the main repo is the source of truth
  for behaviour. A change that diverges from the contract needs a reason in the
  PR.
- The main repo's
  [`CONTRIBUTING.md`](https://github.com/wispr-flow-linux/wispr-flow-linux/blob/main/CONTRIBUTING.md)
  and [`MAINTAINING.md`](https://github.com/wispr-flow-linux/wispr-flow-linux/blob/main/MAINTAINING.md)
  apply here too: what we accept, the PR policy, AI attribution, releases.
- The main repo's `docs/learnings/` entries for the subsystem you touch:
  `wayland-injection.md`, `global-key-monitor.md`, `kwin-zbus-tokio.md`,
  `gnome-shell-extension.md`, `helper-spawn-env.md`.

## Clean room

No Wispr Flow code, decompiled or otherwise, goes in this repo. The helper is
built only from the documented IPC contract. Released into the public domain
([UNLICENSE](UNLICENSE)).

## Layout

```
src/main.rs               stdin reader, fd 3 event writer, command dispatch
src/proto.rs              envelope + '|' framing with '+' escaping (+ tests)
src/keymap.rs             Windows VK -> X11 keysym and Linux evdev KEY_*
src/paste.rs               per-app paste chord: paste-rules.json, terminal detection
src/paste_cli.rs           `paste-rules` subcommand that edits that file
src/capture/              global key monitor: xinput.rs (X11), evdev.rs (Wayland)
src/backend/mod.rs        Backend trait, detect(): Wayland > X11 > stub
src/backend/x11.rs        XTEST injection, _NET_* window info, xclip/xsel
src/backend/wayland.rs    uinput injection + clipboard + per-compositor app info
src/backend/uinput.rs     in-process /dev/uinput keyboard, held-modifier handling
src/backend/wl_clipboard.rs  ext-data-control text/plain + text/html clipboard
src/backend/kwin.rs       KDE active window + focus events (zbus + KWin script)
src/backend/gnome.rs      GNOME active window + focus events via the extension
src/backend/gnome_extension/  the bundled GNOME Shell extension (JS)
src/backend/atspi_*.rs    AT-SPI active-app and selection fallbacks
src/backend/stub.rs       no-op backend that keeps the handshake alive
*.py, vm_*.sh             manual harnesses; they inject into the focused window
```

## Rules that bite

- Use async `zbus` on tokio for D-Bus services, never `zbus::blocking`
  (see `kwin-zbus-tokio.md`; it deadlocked KDE active-app once).
- Unhandled commands ACK as safe no-ops so the app does not relaunch-loop the
  helper. Keep that true for every new command.
- A synthetic chord must leave the user's physically held modifiers exactly as
  it found them. Stuck Ctrl/Alt/Shift after a paste is the most reported bug
  class; any change to `uinput.rs` or chord timing needs a test for it.
- The release binary must run on Ubuntu 22.04 (glibc 2.35). `release.yml`
  fails the build otherwise; don't add a dependency that raises the floor.
- The manual harnesses (`live_inject_test.py`, `vm_validate.py`) type into
  whatever window has focus. Don't run them on a desktop you care about.

## Releasing

Bump `version` in `Cargo.toml` in a PR, merge, then push the matching
`vX.Y.Z` tag on `main`; `release.yml` publishes the x86_64 and aarch64
binaries. The main repo then pins the tag in `helper-version.txt` and the
`rev`/hash in `nix/wispr-flow.nix`. Merging and tagging wait for the
maintainer (see `MAINTAINING.md`).

## Issues, PRs, and commits

Same conventions as the main repo: short and plain, `Fixes #123`, evidence in
the commit body, no headers or checklists. AI-assisted PRs end with a
`Co-Authored-By: Claude <model> <noreply@anthropic.com>` trailer; commits
carry `Co-Authored-By: Claude <claude@anthropic.com>`. User-facing bugs are
filed in the main repo; issues here are for code-level helper work.
