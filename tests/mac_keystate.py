#!/usr/bin/env python3
"""Print every key transition seen by the Mac this runs on, left and right
modifiers apart.

Modifiers come from the session's device-specific modifier flags: macOS folds a
right-hand modifier onto the left keycode in the per-key state
(CGEventSourceKeyState reports keycode 55 for a right Command press), but the
NX_DEVICE* flag bits keep the sides distinct. Every other key is polled by
keycode. Run it in a Terminal on the Mac, in the logged-in GUI session, then
press keys through remotex:

    python3 tests/mac_keystate.py

No Accessibility or Input Monitoring permission is needed: these are state
queries, not an event tap. Ctrl-C to stop.
"""
import ctypes
import sys
import time

cg = ctypes.cdll.LoadLibrary(
    "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics"
)
cg.CGEventSourceKeyState.restype = ctypes.c_bool
cg.CGEventSourceKeyState.argtypes = [ctypes.c_int32, ctypes.c_uint16]
cg.CGEventSourceFlagsState.restype = ctypes.c_uint64
cg.CGEventSourceFlagsState.argtypes = [ctypes.c_int32]
COMBINED_SESSION_STATE = 0

# IOLLEvent.h device-dependent modifier bits: one per physical key.
MODIFIER_FLAGS = {
    0x00000001: "Control LEFT",
    0x00002000: "Control RIGHT",
    0x00000002: "Shift LEFT",
    0x00000004: "Shift RIGHT",
    0x00000008: "Command LEFT",
    0x00000010: "Command RIGHT",
    0x00000020: "Option/Alt LEFT",
    0x00000040: "Option/Alt RIGHT",
}
# Keycodes the flags already cover; their folded per-key state is ignored.
MODIFIER_KEYCODES = {54, 55, 56, 58, 59, 60, 61, 62}

NAMES = {
    63: "Fn", 57: "CapsLock",
    0: "a", 1: "s", 2: "d", 3: "f", 4: "h", 5: "g", 6: "z", 7: "x", 8: "c",
    9: "v", 11: "b", 12: "q", 13: "w", 14: "e", 15: "r", 16: "y", 17: "t",
    18: "1", 19: "2", 20: "3", 21: "4", 22: "6", 23: "5", 24: "=", 25: "9",
    26: "7", 27: "-", 28: "8", 29: "0", 30: "]", 31: "o", 32: "u", 33: "[",
    34: "i", 35: "p", 36: "Return", 37: "l", 38: "j", 39: "'", 40: "k",
    41: ";", 42: "\\", 43: ",", 44: "/", 45: "n", 46: "m", 47: ".", 48: "Tab",
    49: "Space", 50: "`", 51: "Delete", 53: "Escape", 96: "F5", 97: "F6",
    98: "F7", 99: "F3", 100: "F8", 101: "F9", 103: "F11", 109: "F10",
    111: "F12", 118: "F4", 120: "F2", 122: "F1", 114: "Help/Insert",
    115: "Home", 116: "PageUp", 117: "ForwardDelete", 119: "End",
    121: "PageDown", 123: "Left", 124: "Right", 125: "Down", 126: "Up",
}


def name(code: int) -> str:
    return NAMES.get(code, f"keycode {code}")


def snapshot() -> dict[str, bool]:
    flags = cg.CGEventSourceFlagsState(COMBINED_SESSION_STATE)
    state = {label: bool(flags & bit) for bit, label in MODIFIER_FLAGS.items()}
    for k in range(128):
        if k not in MODIFIER_KEYCODES:
            state[f"{name(k)} (keycode {k})"] = cg.CGEventSourceKeyState(
                COMBINED_SESSION_STATE, k
            )
    return state


def main() -> None:
    sys.stdout.reconfigure(line_buffering=True)
    down = snapshot()
    print("watching; currently held:", [k for k, v in down.items() if v] or "nothing")
    while True:
        now = snapshot()
        for key, pressed in now.items():
            if pressed != down[key]:
                down[key] = pressed
                held = [k for k, v in down.items() if v]
                print(f"{'DOWN' if pressed else 'UP  '} {key:24} held: {held or '-'}")
        time.sleep(0.01)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
