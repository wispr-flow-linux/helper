#!/usr/bin/env python3
"""Exercise real helper IPC + real PipeWire, restoring baseline in finally."""
import json
import os
import select
import subprocess
import sys
import time

BINARY = sys.argv[1]


def pactl(*args):
    return subprocess.check_output(["/usr/bin/pactl", *args], text=True, timeout=5)


def sinks():
    return {
        s["name"]: {"mute": s["mute"], "volume": s["volume"]}
        for s in json.loads(pactl("-f", "json", "list", "sinks"))
    }


def wait_for(predicate):
    end = time.monotonic() + 5
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("Timed out waiting for audio state")


class Helper:
    def __init__(self):
        r, w = os.pipe()
        if r == 3:
            r2 = os.dup(r)
            os.close(r)
            r = r2
        if w != 3:
            os.dup2(w, 3)
            os.close(w)
        self.r = r
        self.buf = b""
        self.p = subprocess.Popen(
            [BINARY],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            pass_fds=(3,),
            env={**os.environ, "DISPLAY": "", "WAYLAND_DISPLAY": ""},
        )
        os.close(3)

    def command(self, command):
        uid = str(time.monotonic_ns())
        msg = (
            json.dumps({"HelperAPIRequest": {command: True, "uuid": uid}})
            .replace("+", "+1")
            .replace("|", "+2")
            + "|"
        )
        self.p.stdin.write(msg.encode())
        self.p.stdin.flush()
        end = time.monotonic() + 8
        while time.monotonic() < end:
            if b"|" not in self.buf:
                ready, _, _ = select.select([self.r], [], [], 1)
                if not ready:
                    continue
                data = os.read(self.r, 65536)
                if not data:
                    raise AssertionError("helper IPC closed")
                self.buf += data
            while b"|" in self.buf:
                raw, self.buf = self.buf.split(b"|", 1)
                obj = json.loads(raw.decode().replace("+2", "|").replace("+1", "+"))
                response = obj.get("HelperAPIResponse", {})
                if response.get("uuid") == uid:
                    assert response.get("ACK") is True, response
                    return
        raise AssertionError("helper did not ACK")

    def close(self, mode="eof"):
        if self.p.poll() is None:
            if mode == "kill":
                self.p.kill()
            elif mode == "term":
                self.p.terminate()
            else:
                self.p.stdin.close()
            self.p.wait(timeout=8)
        os.close(self.r)


original = sinks()
modules = []
helpers = []
try:
    for name, muted in [
        ("wispr_patch_test_open", False),
        ("wispr_patch_test_muted", True),
    ]:
        modules.append(
            pactl("load-module", "module-null-sink", f"sink_name={name}").strip()
        )
        pactl("set-sink-mute", name, "1" if muted else "0")
    baseline = sinks()
    h = Helper()
    helpers.append(h)
    h.command("IsReady")
    h.command("DictationStart")
    assert all(s["mute"] for s in sinks().values())
    h.command("DictationStart")
    modules.append(
        pactl(
            "load-module", "module-null-sink", "sink_name=wispr_patch_test_hotplug"
        ).strip()
    )
    wait_for(lambda: sinks().get("wispr_patch_test_hotplug", {}).get("mute") is True)
    h.command("DictationStop")
    h.command("DictationStop")
    assert {k: v for k, v in sinks().items() if k in baseline} == baseline
    assert not sinks()["wispr_patch_test_hotplug"]["mute"]
    print(
        "PASS: all outputs mute, duplicate start/stop, hotplug, exact mute/volume restoration",
        flush=True,
    )
    h.close()
    helpers.remove(h)
    for mode in ["eof", "term", "kill"]:
        before = sinks()
        h = Helper()
        helpers.append(h)
        h.command("DictationStart")
        assert all(s["mute"] for s in sinks().values())
        h.close(mode)
        helpers.remove(h)
        wait_for(lambda: sinks() == before)
        print(f"PASS: restoration after helper {mode}", flush=True)
    h = Helper()
    helpers.append(h)
    h.command("DictationStart")
    h.command("HelperAppShutdown")
    h.close()
    helpers.remove(h)
    wait_for(lambda: all(sinks()[k] == v for k, v in baseline.items()))
    print("PASS: HelperAppShutdown restoration", flush=True)
finally:
    for h in helpers:
        try:
            h.close()
        except Exception:
            pass
    for name, state in original.items():
        try:
            pactl("set-sink-mute", name, "1" if state["mute"] else "0")
        except Exception:
            pass
    for module in reversed(modules):
        try:
            pactl("unload-module", module)
        except Exception:
            pass
    assert sinks() == original, "Original audio state was not restored"
    print("Original audio state restored; temporary test outputs removed.", flush=True)
