# wispr-flow-linux-helper

> Standalone repo (`github.com/wispr-flow-linux/helper`), split out of the
> `wispr-flow-linux` monorepo. Tagged `v*` releases publish prebuilt
> `wispr-flow-linux-helper-x86_64` and `wispr-flow-linux-helper-aarch64`
> binaries as Release assets, which the main `wispr-flow-linux` package build
> downloads instead of compiling the helper itself.

Clean-room Linux helper for Wispr Flow. It is a standalone process that speaks the
helper IPC contract the Wispr Flow Electron app already uses for its macOS (Swift)
and Windows (C#) helpers, and backs the OS-integration commands with **X11 and
Wayland** backends. The app ships no Linux helper; this fills that gap.

The Wayland backend injects via an **in-process `/dev/uinput` virtual keyboard**
(no `ydotoold` daemon, no root — just `/dev/uinput` write access from the logind
`uaccess` ACL) + `wl-clipboard`. **`PasteText` is live-validated** inserting text
into a focused native KDE Plasma Wayland app.

**Contract is the source of truth:**
[`docs/reference/ipc-contract.md`](https://github.com/wispr-flow-linux/wispr-flow-linux/blob/main/docs/reference/ipc-contract.md)
(+ `keycodes.json`, `commands.json`), kept in the main `wispr-flow-linux` repo.
Recovered directly from the shipped Electron bundle — not guessed.

## What works

| Command | X11 backend | Wayland backend |
|---|---|---|
| `IsReady` → `ACK` | ✅ handshake + keepalive | ✅ |
| `PasteText` | ✅ clipboard (`xclip`/`xsel`) + XTEST Ctrl+V | ✅ **live-validated** — in-process text/plain+text/html clipboard + uinput Ctrl+V |
| `SimulateKeyPress` | ✅ VK→keysym→keycode + XTEST | ✅ VK→evdev + uinput chord (held-modifier snapshot/release) |
| `GetActiveAppInfo` / `GetAppInfo` | ✅ `_NET_ACTIVE_WINDOW`→PID/`WM_CLASS` | ✅ **KDE** via KWin script bridge; ⬜ other compositors |
| `GetRunningApps` | ✅ `_NET_CLIENT_LIST` | ⚠️ KDE: active app only (full list TBD); ⬜ other |
| `SetFocusChangeDetectorState` → `AppInfoUpdate` | ⬜ (TODO: `PropertyNotify`) | ✅ **KDE** — focus events on fd 3, gated & deduped |
| `GetSelectedTextViaCopy` | ⚠️ Ctrl+C copy-probe | ⚠️ Ctrl+C copy-probe |
| `GetAccessibilityStatus` | ✅ (connection live) | ✅ (uinput live) |
| everything else (intervals/BLE/panel/analytics…) | ACK no-op | ACK no-op |

`detect()` picks Wayland when `$WAYLAND_DISPLAY` is set and `/dev/uinput` is
writable, else X11 (`$DISPLAY`), else a no-op stub.

Design choice: unhandled commands are ACK'd as safe no-ops so the unmodified app
stays healthy instead of relaunch-looping the helper. See `src/main.rs` dispatch.

## Build

Needs a Rust toolchain (not currently installed on this machine):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if needed
cargo build            # debug
cargo build --release  # single stripped binary -> target/release/wispr-flow-linux-helper
cargo test             # framing roundtrip + decoder tests (proto.rs)
```

`x11rb` is pure-Rust (speaks the X11 wire protocol over the socket), so no
`libxcb`/`libX11` dev headers are required. For the clipboard baseline, install
`xclip` (or `xsel`). XTEST must be enabled on the X server (it is by default).

## Test without the full app

`test_harness.py` mimics Electron's spawn (4 stdio pipes: commands on stdin,
events on **fd 3**) and runs a scripted conversation:

```bash
cargo build
python3 test_harness.py            # uses ./target/debug/wispr-flow-linux-helper
RUST_LOG=debug python3 test_harness.py ./target/release/wispr-flow-linux-helper
```

Expected: an `ACK` for `IsReady`, an `ActiveAppInfo` for the focused window, a
`RunningApps` list, and an `AccessibilityStatus`. (PasteText/SimulateKeyPress are
commented out in the harness because they inject into the focused window.)

**Live injection test** (`live_inject_test.py`) exercises the real PasteText +
chord path against a focused editor:

```bash
# launches its own kate:
python3 live_inject_test.py target/release/wispr-flow-linux-helper
# or inject into whatever you already have focused (keep it focused ~5s):
python3 live_inject_test.py target/release/wispr-flow-linux-helper none
```

It PasteTexts a marker, overwrites the clipboard with a sentinel, then Ctrl+A/Ctrl+C
to read the editor back. The automated readback has a clipboard-owner race that can
report a false negative — the paste landing is verifiable by eye in the editor.

## Wiring into the app (Phase 0 packaging)

One mandatory patch to the unmodified Electron main bundle: the helper-path
resolver is a two-way `isMac ? mac : windows` switch with **no Linux case**
(ipc-contract.md §8). Add a `'linux'` branch pointing at this binary, staged
under `resources/Release/` (or wherever the Linux build places it), and spawn it
with `stdio:["pipe","pipe","pipe","pipe"]` (the app already does this).

## Layout

```
src/
  main.rs            entry: stdin reader, fd3 writer, dispatch, IsReady/ACK
  proto.rs           envelope + framing (escape '+'/'|', delimiter '|') + tests
  keymap.rs          Windows VK -> X11 keysym AND -> Linux evdev KEY_* (from keycodes.json)
  backend/
    mod.rs           Backend trait + types + detect() (Wayland > X11 > stub)
    x11.rs           X11 implementation (XTEST + _NET_* + xclip/xsel)
    wayland.rs       Wayland implementation (uinput injection + clipboard + KWin)
    uinput.rs        in-process /dev/uinput virtual keyboard + held-modifier snapshot
    wl_clipboard.rs  in-process text/plain+text/html clipboard (ext_data_control)
    kwin.rs          KDE active-window bridge + focus-event source (zbus + KWin script)
    stub.rs          no-op fallback (keeps handshake alive on unsupported sessions)
test_harness.py      Electron stand-in: scripted handshake/info conversation
live_inject_test.py  live PasteText + Ctrl+A/Ctrl+C round-trip against a focused editor
focus_test.py        focus-event (AppInfoUpdate) streaming + SetFocusChangeDetectorState gating
clipboard_test.py    in-process clipboard offers text/plain + text/html
hotplug_test.py      evdev capture adopts / re-adopts keyboards appearing after startup
```

## Roadmap (next, in priority order)

1. ✅ **KDE active-app identity + focus events** — done (`backend/kwin.rs`): KWin
   script pushes `windowActivated` → zbus service → cache + `AppInfoUpdate` events
   on fd 3 (gated by `SetFocusChangeDetectorState`).
2. ✅ **Held-modifier snapshot/restore** (Wayland) — done (`backend/uinput.rs`),
   guarded on `/dev/input` read access. TODO: X11 `XQueryKeymap` equivalent.
3. ✅ **text/plain + text/html clipboard** (Wayland) — done (`backend/wl_clipboard.rs`,
   `ext_data_control`). TODO: X11 in-process selection owner; prior-clipboard
   save/restore (read side still uses `wl-paste`).
4. **Full `GetRunningApps` on KDE** — walk `workspace.windowList` in the KWin script.
   **GNOME path** — shell-extension equivalent of the KWin bridge.
5. **AT-SPI selection** — replace the copy-probe with real `atspi` Text-interface
   reads (`GetSelectedTextViaCopy` without synthetic Ctrl+C). Both backends.
6. **Focus tracking on X11** — `PropertyNotify` on `_NET_ACTIVE_WINDOW` → `AppInfoUpdate`.
7. **codingCliAgent detection** — terminal + running-process heuristics for the
   `ActiveAppInfo.codingCliAgent` enum.

## Legal

Clean-room reimplementation against a recovered IPC contract; ships no Wispr Flow
proprietary code, and is released into the public domain under the
[Unlicense](UNLICENSE). The app itself remains under its own terms — see the
[legal posture](https://github.com/wispr-flow-linux/wispr-flow-linux#legal-posture)
in the main repo.
