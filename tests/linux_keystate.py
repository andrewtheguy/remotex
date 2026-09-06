#!/usr/bin/env python3
"""Print every X11 key transition on the display this runs against.

The Linux twin of mac_keystate.py: polls XQueryKeymap, so it needs no focus, no
root and no packages, and names each keycode through the server's own keymap
(Alt_L vs Alt_R, Control_L vs Control_R, Super_L vs Super_R). Run it inside the
remote X session (TigerVNC, xrdp), e.g. in a terminal there or over ssh with
DISPLAY set:

    DISPLAY=:1 python3 tests/linux_keystate.py

Ctrl-C to stop.
"""
import ctypes
import ctypes.util
import os
import sys
import time

x11 = ctypes.cdll.LoadLibrary(ctypes.util.find_library("X11") or "libX11.so.6")
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XQueryKeymap.argtypes = [ctypes.c_void_p, ctypes.c_char * 32]
x11.XKeycodeToKeysym.restype = ctypes.c_ulong
x11.XKeycodeToKeysym.argtypes = [ctypes.c_void_p, ctypes.c_ubyte, ctypes.c_int]
x11.XKeysymToString.restype = ctypes.c_char_p
x11.XKeysymToString.argtypes = [ctypes.c_ulong]


def main() -> None:
    sys.stdout.reconfigure(line_buffering=True)
    display = x11.XOpenDisplay(None)
    if not display:
        sys.exit(f"cannot open display {os.environ.get('DISPLAY')!r}; set DISPLAY to the remote session's")

    def name(code: int) -> str:
        sym = x11.XKeycodeToKeysym(display, code, 0)
        text = x11.XKeysymToString(sym) if sym else None
        return text.decode() if text else f"keycode {code}"

    def snapshot() -> list[bool]:
        keys = (ctypes.c_char * 32)()
        x11.XQueryKeymap(display, keys)
        raw = bytes(keys)
        return [bool(raw[k >> 3] & (1 << (k & 7))) for k in range(256)]

    down = snapshot()
    print("watching; currently held:", [name(k) for k in range(256) if down[k]] or "nothing")
    while True:
        now = snapshot()
        for k in range(256):
            if now[k] != down[k]:
                down[k] = now[k]
                still = [name(j) for j in range(256) if down[j]]
                print(f"{'DOWN' if now[k] else 'UP  '} {name(k):14} (keycode {k:3})   held: {still or '-'}")
        time.sleep(0.01)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
