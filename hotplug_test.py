#!/usr/bin/env python3
"""Keyboard hotplug test: does evdev capture pick up a keyboard that appears
*after* startup, and re-adopt one that goes away and comes back?

Regression guard for the bug where `start()` enumerated /dev/input once and a
reader thread lost to ENODEV was never replaced: a USB hub that re-enumerates
across suspend/resume (dock, wireless dongle) silently left the helper deaf to
every external keyboard until the app restarted.

Creates a dummy uinput keyboard, destroys it, recreates it, and checks the
helper's own stderr for `evdev capture: watching ...` / `read ended: ...`. The
dummy device never emits a key event, so nothing is injected anywhere.

Needs write access to /dev/uinput (the logind uaccess ACL gives the active
session it; no root required) and a session where evdev capture is the chosen
backend -- i.e. Wayland, or X11 where XInput2 is unavailable.

Usage: python3 hotplug_test.py [path-to-helper]
"""
import fcntl, os, re, struct, subprocess, sys, threading, time

UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502
UI_SET_EVBIT, UI_SET_KEYBIT = 0x40045564, 0x40045565
EV_KEY = 0x01
NAME = "hotplug-test-keyboard"
# Long enough for the 2s rescan plus udev mknod + the logind ACL landing on it.
SETTLE = 5.0

log = []
def reader_thread(stream):
    for line in stream:
        log.append(line.rstrip())

def seen(pattern, since=0):
    rx = re.compile(pattern)
    return [l for l in log[since:] if rx.search(l)]

def node_for(name):
    """The eventN backing the input device called `name`, or None."""
    cur = None
    for line in open("/proc/bus/input/devices"):
        if line.startswith("N: Name="):
            cur = line.split('"')[1]
        elif line.startswith("H: Handlers=") and cur == name:
            for tok in line.split("=", 1)[1].split():
                if tok.startswith("event"):
                    return "/dev/input/" + tok
    return None

def create_keyboard():
    """A uinput keyboard udev will classify ID_INPUT_KEYBOARD (so it gets the
    uaccess ACL and is actually readable). Returns (fd, node)."""
    fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    for code in range(1, 256):
        fcntl.ioctl(fd, UI_SET_KEYBIT, code)
    dev = NAME.encode().ljust(80, b"\0") + struct.pack("<4H", 0x03, 0xDEAD, 0xBEEF, 1)
    dev += struct.pack("<I", 0) + b"\0" * (4 * 64 * 4)
    os.write(fd, dev)
    fcntl.ioctl(fd, UI_DEV_CREATE)
    for _ in range(60):
        node = node_for(NAME)
        if node and os.access(node, os.R_OK):
            return fd, node
        time.sleep(0.05)
    os.close(fd)
    raise RuntimeError("dummy keyboard never became readable")

def destroy_keyboard(fd):
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    os.close(fd)

def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "./target/release/wispr-flow-linux-helper"
    devnull = os.open(os.devnull, os.O_WRONLY)
    proc = subprocess.Popen(
        [binary],
        stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        pass_fds=(devnull,), text=True,
        env={**os.environ, "RUST_LOG": "info"},
        preexec_fn=lambda: os.dup2(devnull, 3),  # fd 3 is the event channel
    )
    threading.Thread(target=reader_thread, args=(proc.stderr,), daemon=True).start()
    failures = []
    try:
        time.sleep(3)
        if not seen(r"key capture: evdev"):
            print("SKIP: evdev is not the active capture backend here")
            return 0
        print(f"startup: {len(seen(r'evdev capture: watching'))} keyboard(s) adopted")

        # 1. a keyboard that did not exist at startup
        mark = len(log)
        fd, node = create_keyboard()
        print(f"created {node}")
        time.sleep(SETTLE)
        if seen(rf"watching {re.escape(node)}$", mark):
            print(f"PASS  adopted {node} after startup")
        else:
            failures.append(f"never adopted {node} after startup")

        # 2. its reader must end when the device goes away
        mark = len(log)
        destroy_keyboard(fd)
        time.sleep(SETTLE)
        if seen(rf"{re.escape(node)} read ended", mark):
            print(f"PASS  reader for {node} ended on removal")
        else:
            failures.append(f"reader for {node} did not end on removal")

        # 3. the regression: same path comes back, must be re-adopted
        mark = len(log)
        fd, node2 = create_keyboard()
        print(f"recreated {node2}")
        time.sleep(SETTLE)
        if seen(rf"watching {re.escape(node2)}$", mark):
            print(f"PASS  re-adopted {node2} after re-enumeration")
        else:
            failures.append(f"never re-adopted {node2} after re-enumeration")
        destroy_keyboard(fd)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        os.close(devnull)

    if failures:
        print("\nFAIL:")
        for f in failures:
            print(f"  - {f}")
        print("\nhelper stderr:")
        for l in log:
            print(f"  {l}")
        return 1
    print("\nall checks passed")
    return 0

if __name__ == "__main__":
    sys.exit(main())
