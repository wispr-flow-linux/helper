#!/usr/bin/env python3
"""Live end-to-end injection test for the Wayland/X11 backend (deterministic).

Proves the full path uinput/XTEST -> compositor -> focused GUI app by writing to
a real file through a text editor:

  1. Create an empty temp file and open it in a GUI editor (auto-detected, or
     pass one explicitly). Opening a *file* avoids editors' "welcome" screens
     where the text area isn't focused (KWrite/Kate/gnome-text-editor do this).
  2. PasteText "<MARKER>" via the helper (sets clipboard + injects Ctrl+V).
  3. SimulateKeyPress Ctrl+S via the helper (a multi-modifier chord) to save.
  4. Read the file back: PASS iff it contains the marker.

This is deterministic (no clipboard-readback race): the marker only reaches the
file if both the paste injection and the Ctrl+S chord actually landed in the
focused editor.

Usage: python3 live_inject_test.py [path-to-helper] [editor|auto|none]
Requires a live graphical session, injection access (/dev/uinput on Wayland or
XTEST on X11) and wl-clipboard (Wayland) / xclip (X11).
"""
import json
import os
import shutil
import subprocess
import sys
import threading
import time

DELIM = b"|"
MARKER = "WISPR-PASTE-OK-4F2A café è 中文 🎙 + | apostrophe's\nSecond line"

# Editors that (a) take a file path arg, (b) put the cursor in the document, and
# (c) save the open file with Ctrl+S without a dialog. Ordered by preference.
EDITOR_CANDIDATES = [
    "kwrite", "kate",                      # KDE / Qt
    "gnome-text-editor", "gedit",          # GNOME / GTK
    "mousepad", "xed", "featherpad",       # XFCE / Cinnamon / LXQt
    "leafpad",
]


def escape(s): return s.replace("+", "+1").replace("|", "+2")
def unescape(s): return s.replace("+2", "|").replace("+1", "+")
def frame(env): return escape(json.dumps(env, indent=2)).encode() + DELIM


def reader_thread(rfd, events):
    buf = b""
    with os.fdopen(rfd, "rb", buffering=0) as f:
        while True:
            chunk = f.read(4096)
            if not chunk:
                return
            buf += chunk
            while DELIM in buf:
                raw, buf = buf.split(DELIM, 1)
                if not raw:
                    continue
                try:
                    events.append(json.loads(unescape(raw.decode("utf-8", "replace"))))
                except Exception:
                    pass


def pick_editor(arg):
    if arg and arg not in ("auto", "none"):
        return arg
    if arg == "none":
        return None
    for cand in EDITOR_CANDIDATES:
        if shutil.which(cand):
            return cand
    return None


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "./target/release/wispr-flow-linux-helper"
    editor_arg = sys.argv[2] if len(sys.argv) > 2 else "auto"
    if not os.path.exists(binary):
        sys.exit(f"helper not found: {binary}")

    editor = pick_editor(editor_arg)
    if editor is None and editor_arg != "none":
        print("⚠️  no GUI editor found among", EDITOR_CANDIDATES)
        print("   install one (e.g. gedit) or pass an editor explicitly.")
        return 3

    target = f"/tmp/wispr_inject_{os.getpid()}.txt"
    try:
        os.remove(target)
    except OSError:
        pass
    open(target, "w").close()

    # fd3 plumbing (same as test_harness.py)
    r3, w3 = os.pipe()
    if r3 == 3:
        r3n = os.dup(r3); os.close(r3); r3 = r3n
    if w3 != 3:
        os.dup2(w3, 3); os.close(w3)
    os.set_inheritable(3, True)

    proc = subprocess.Popen(
        [binary], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=None,
        pass_fds=(3,), env={**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "info")},
    )
    os.close(3)
    events = []
    threading.Thread(target=reader_thread, args=(r3, events), daemon=True).start()

    u = [0]
    def send(inner, label):
        u[0] += 1
        inner = {**inner, "uuid": f"t-{u[0]}"}
        print(f"  -> {label}")
        proc.stdin.write(frame({"HelperAPIRequest": inner})); proc.stdin.flush()

    print("[1] handshake")
    send({"IsReady": True}, "IsReady")
    time.sleep(0.4)

    ed = None
    if editor is None:
        print("[2] using already-focused window (no launch); injecting in 3s — keep an editor focused")
        time.sleep(3.0)
    else:
        print(f"[2] launching editor on file: {editor} {target}")
        ed = subprocess.Popen([editor, target], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(4.0)  # let it map, open the file, and focus the text area

    print("[3] PasteText marker -> editor (Ctrl+V)")
    send({"PasteText": {"payload": {"text": MARKER, "htmlText": "", "transcriptEntityUUID": ""}}}, "PasteText")
    time.sleep(1.0)

    print("[4] SimulateKeyPress Ctrl+S (save)")
    send({"SimulateKeyPress": {"payload": {"keycode": 83, "flags": ["Control"]}}}, "Ctrl+S")
    time.sleep(1.5)

    # cleanup the editor before reading (some editors flush on quit)
    if ed is not None:
        ed.terminate()
        try: ed.wait(timeout=3)
        except Exception: ed.kill()
    time.sleep(0.3)

    try:
        with open(target, "r") as f:
            got = f.read()
    except OSError as e:
        got = ""
        print(f"  (could not read target: {e})")
    print(f"\n[result] file {target} contents = {got!r}")

    proc.stdin.close()
    try: proc.wait(timeout=2)
    except Exception: proc.terminate()
    try: os.remove(target)
    except OSError: pass

    print(f"[events] {len(events)} fd3 messages; responses: "
          + ", ".join(sorted({k for e in events for k in e.get('HelperAPIResponse', {}) if k != 'uuid'})))

    if MARKER in got:
        print("\n✅ PASS — PasteText Ctrl+V landed in the focused app AND the Ctrl+S chord saved it.")
        return 0
    else:
        print("\n❌ FAIL — marker not in the file: injection did not reach the editor "
              "(focus? injection perms? editor lacks Ctrl+S-to-file?).")
        return 1


if __name__ == "__main__":
    sys.exit(main())
