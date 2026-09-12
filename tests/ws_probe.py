#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets", "requests"]
# ///
"""Drive a local gateway WebSocket and print the control messages a browser sees.

This is a manual probe for display selection and dynamic-resolution behavior. Start
``remotex serve`` separately, then run, for example:

    REMOTEX_PROBE_PASSWORD=... uv run tests/ws_probe.py \
        --port 52675 --target sandbox2highperf --user admin \
        --viewport 1366x768 --viewport 1920x1080

Use ``--burst`` to send every requested viewport without waiting for the preceding
resize response.
"""

import argparse
import asyncio
import getpass
import json
import math
import os
import sys
import time

import requests
import websockets


def dimensions(value: str) -> tuple[int, int]:
    """Parse a positive WIDTHxHEIGHT argument."""
    try:
        width, height = map(int, value.lower().split("x", maxsplit=1))
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected WIDTHxHEIGHT") from error
    if width <= 0 or height <= 0:
        raise argparse.ArgumentTypeError("dimensions must be positive")
    return width, height


def host_display(value: str) -> dict[str, int | bool]:
    """Parse a WIDTHxHEIGHT@SCALE[fit] client screen: scale in hundredths (200 =
    Retina), a trailing ``fit`` for the pinch-zoom client."""
    try:
        size, scale = value.lower().split("@", maxsplit=1)
        fit = scale.endswith("fit")
        width, height = dimensions(size)
        hundredths = int(scale.removesuffix("fit"))
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected WIDTHxHEIGHT@SCALE[fit]") from error
    if hundredths <= 0:
        raise argparse.ArgumentTypeError("scale must be positive")
    return {"w": width, "h": height, "scale": hundredths, "fit": fit}


def duration(value: str) -> float:
    """Parse a non-negative number of seconds."""
    try:
        seconds = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a number of seconds") from error
    if seconds < 0:
        raise argparse.ArgumentTypeError("seconds must not be negative")
    return seconds


def coordinates(value: str) -> tuple[int, int]:
    """Parse an X,Y argument."""
    try:
        x, y = map(int, value.split(",", maxsplit=1))
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected X,Y") from error
    return x, y


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=52675)
    parser.add_argument("--target", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument(
        "--password",
        default=os.environ.get("REMOTEX_PROBE_PASSWORD"),
        help="gateway password (prefer REMOTEX_PROBE_PASSWORD)",
    )
    parser.add_argument("--seconds", type=float, default=25.0)
    parser.add_argument(
        "--reconnect-target",
        default=None,
        help="after --reconnect-after seconds, send a second connect to this target "
        "with no disconnect in between (the gateway ends the running session first)",
    )
    parser.add_argument("--reconnect-after", type=float, default=6.0)
    parser.add_argument(
        "--select",
        type=lambda value: int(value, 0),
        action="append",
        default=[],
        help="display id to select once the list arrives (repeatable)",
    )
    parser.add_argument("--mouse", type=coordinates, default=None)
    parser.add_argument(
        "--display",
        type=host_display,
        default=None,
        help="client screen WIDTHxHEIGHT@SCALE[fit] carried on the connect (the opening size)",
    )
    parser.add_argument("--mouse-width", type=int, default=None)
    parser.add_argument(
        "--sweep",
        type=duration,
        default=None,
        help="after the first resize, sweep the pointer in a circle around the "
        "desktop's centre for this many seconds (about 60 moves a second) — the "
        "motion a damage tape of pointer movement is recorded from",
    )
    parser.add_argument(
        "--page",
        type=duration,
        default=None,
        help="after the first resize, tap Home, then PageDown for two seconds and "
        "PageUp for two, about six taps a second, for this many seconds — the scroll a damage "
        "tape of paging through a long document is recorded from",
    )
    parser.add_argument(
        "--viewport",
        type=dimensions,
        action="append",
        default=[],
        help="viewport WIDTHxHEIGHT in points to request after the display list arrives (repeatable)",
    )
    parser.add_argument(
        "--key",
        action="append",
        default=[],
        metavar="CODE",
        help="DOM key code to press once the desktop is up (repeatable: all are "
        "held together as one chord, pressed in order and released in reverse)",
    )
    parser.add_argument(
        "--chord",
        action="append",
        default=[],
        metavar="CODE[+CODE...]",
        help="a chord of DOM key codes, pressed in order and released in reverse "
        "(repeatable: each --chord is sent in turn, a moment apart, after --key-delay). "
        "Where --key holds one chord down for --key-hold, this taps a sequence — what "
        "driving a dialog on the remote takes",
    )
    parser.add_argument(
        "--key-delay",
        type=duration,
        default=8.0,
        help="seconds after the first resize before the --key chord goes down "
        "(Apple Screen Sharing drops input sent in a session's first seconds)",
    )
    parser.add_argument(
        "--key-hold",
        type=duration,
        default=0.3,
        help="seconds the --key chord stays down",
    )
    parser.add_argument(
        "--chord-gap",
        type=duration,
        default=1.0,
        help="seconds between one --chord and the next, which is how long whatever "
        "the last one opened on the remote gets to appear",
    )
    parser.add_argument(
        "--clipboard",
        metavar="TEXT",
        help="drive the Clipboard panel once the desktop is up: fetch what the remote "
        "holds, then put TEXT on its clipboard, and report every clipboard message "
        "that comes back. A push is not echoed — what the remote does with it is a "
        "paste over there, which shows up as nothing here and as the text in whatever "
        "was pasted into",
    )
    parser.add_argument(
        "--burst",
        action="store_true",
        help="send all viewport requests together instead of waiting for each answer",
    )
    parser.add_argument(
        "--viewport-after-resize",
        action="store_true",
        help="send the first --viewport after the first resize instead of after the "
        "display list, which a generic VNC server never sends",
    )
    args = parser.parse_args()

    password = args.password or getpass.getpass("Gateway password: ")
    base = f"http://127.0.0.1:{args.port}"
    session = requests.Session()
    login = session.post(
        f"{base}/api/auth/login",
        json={"username": args.user, "password": password},
        timeout=10,
    )
    login.raise_for_status()
    cookie = session.cookies.get("remotex_session")
    claim = session.post(f"{base}/api/session", json={}, timeout=10)
    claim.raise_for_status()
    token = claim.json()["sessionId"]
    print(f"  logged in, session {token[:12]}…")

    # The session socket requires the browser's chroma answer; the probe stands in
    # for a decoder that takes VP9 profile 1, as a desktop browser does.
    url = f"ws://127.0.0.1:{args.port}/ws?session={token}&chroma=444"
    async with websockets.connect(
        url, additional_headers={"Cookie": f"remotex_session={cookie}"}
    ) as socket:
        connect = {"type": "connect", "target": args.target}
        if args.display is not None:
            connect["display"] = args.display
            print(f"  -> connect {args.target} display {args.display}")
        await socket.send(json.dumps(connect))
        pending = list(args.select)
        viewports = list(args.viewport)
        awaiting_viewport = None
        burst_sent = False
        mouse_sent = False
        clipboard_sent = False
        tiles = 0

        # The second connect runs on its own clock, beside the receive loop: a
        # quiet desktop sends nothing for seconds, and the deadline must not wait
        # for an inbound message to be noticed.
        async def reconnect() -> None:
            await asyncio.sleep(args.reconnect_after)
            second = {"type": "connect", "target": args.reconnect_target}
            if args.display is not None:
                second["display"] = args.display
            print(f"  -> connect {args.reconnect_target} (no disconnect first)")
            await socket.send(json.dumps(second))

        reconnect_task = (
            asyncio.create_task(reconnect()) if args.reconnect_target is not None else None
        )

        # One chord, sent after the first resize so the engine's input path is
        # live. Codes go down in the given order and up in reverse, the way the
        # browser's soft keyboard sends a combo.
        async def press_keys() -> None:
            await asyncio.sleep(args.key_delay)
            for code in args.key:
                print(f"  -> key down {code}")
                await socket.send(
                    json.dumps({"type": "key", "code": code, "pressed": True, "caps": False})
                )
            await asyncio.sleep(args.key_hold)
            for code in reversed(args.key):
                print(f"  -> key up   {code}")
                await socket.send(
                    json.dumps({"type": "key", "code": code, "pressed": False, "caps": False})
                )

        # A pointer swept round a circle a third of the desktop's shorter side in
        # radius, a full turn every four seconds: motion whose bounding box grows
        # and moves the way a dragged window's does, without needing anything on
        # the desktop to cooperate.
        async def sweep(width: int, height: int) -> None:
            cx, cy = width / 2, height / 2
            radius = min(width, height) / 3
            started = time.monotonic()
            print(f"  -> sweeping the pointer for {args.sweep:g}s")
            while (elapsed := time.monotonic() - started) < args.sweep:
                angle = elapsed * math.tau / 4
                x = int(cx + radius * math.cos(angle))
                y = int(cy + radius * math.sin(angle))
                await socket.send(json.dumps({"type": "mouseMove", "x": x, "y": y}))
                await asyncio.sleep(1 / 60)
            print("  <- sweep done")

        # PageDown and PageUp in alternating two-second runs, tapped the way a
        # held key repeats: the focused window pages through its document and
        # back, so the same content keeps moving for the whole run.
        async def page() -> None:
            for pressed in (True, False):
                await socket.send(
                    json.dumps({"type": "key", "code": "Home", "pressed": pressed, "caps": False})
                )
                await asyncio.sleep(1 / 12)
            started = time.monotonic()
            print(f"  -> paging for {args.page:g}s")
            while (elapsed := time.monotonic() - started) < args.page:
                code = "PageDown" if int(elapsed // 2) % 2 == 0 else "PageUp"
                for pressed in (True, False):
                    await socket.send(
                        json.dumps({"type": "key", "code": code, "pressed": pressed, "caps": False})
                    )
                    await asyncio.sleep(1 / 12)
            print("  <- paging done")

        # Each --chord tapped in turn. Held and released, never left down: a
        # modifier this end forgets is one the remote desktop keeps, and every
        # keystroke after it arrives wearing it.
        async def press_chords() -> None:
            await asyncio.sleep(args.key_delay)
            for chord in args.chord:
                codes = chord.split("+")
                print(f"  -> chord {'+'.join(codes)}")
                for code in codes:
                    await socket.send(
                        json.dumps(
                            {"type": "key", "code": code, "pressed": True, "caps": False}
                        )
                    )
                    await asyncio.sleep(1 / 60)
                for code in reversed(codes):
                    await socket.send(
                        json.dumps(
                            {"type": "key", "code": code, "pressed": False, "caps": False}
                        )
                    )
                    await asyncio.sleep(1 / 60)
                await asyncio.sleep(args.chord_gap)
            print("  <- chords done")

        keys_task = None
        chords_task = None
        sweep_task = None
        page_task = None
        try:
            async with asyncio.timeout(args.seconds):
                async for message in socket:
                    if isinstance(message, bytes):
                        tiles += 1
                        # Acknowledge the batch at once, as a client that painted it
                        # instantly would: the gateway's paint window holds the next
                        # batch when too many are owed, and a probe that never acked
                        # would stall the engine behind a window that never opens.
                        if len(message) >= 8 and message[0] == 0x02:
                            sequence = int.from_bytes(message[4:8], "little")
                            await socket.send(
                                json.dumps(
                                    {
                                        "type": "paintAck",
                                        "sequence": sequence,
                                        "queuedMs": 0,
                                        "drawMs": 0,
                                    }
                                )
                            )
                        continue
                    data = json.loads(message)
                    kind = data.get("type")
                    if kind == "resize":
                        if args.key and keys_task is None:
                            keys_task = asyncio.create_task(press_keys())
                        if args.chord and chords_task is None:
                            chords_task = asyncio.create_task(press_chords())
                        if args.sweep is not None and sweep_task is None:
                            sweep_task = asyncio.create_task(sweep(data["w"], data["h"]))
                        if args.page is not None and page_task is None:
                            page_task = asyncio.create_task(page())
                        # A viewport is requested in points and answered in pixels at
                        # the announced scale: 1728x883 asked at 2x comes back as
                        # 3456x1766, and is the answer to that request.
                        answered = (
                            round(data["w"] / data["scale"]),
                            round(data["h"] / data["scale"]),
                        )
                        print(
                            f"  resize  {data['w']}x{data['h']}  scale={data['scale']}"
                            f"  tileGrid={data['tileGrid']['w']}x{data['tileGrid']['h']}"
                            f"   -> {data['w'] / data['scale']:g}x"
                            f"{data['h'] / data['scale']:g} CSS px"
                        )
                        if (
                            args.viewport_after_resize
                            and viewports
                            and awaiting_viewport is None
                        ):
                            awaiting_viewport = viewports.pop(0)
                            print(
                                f"  -> viewport {awaiting_viewport[0]}x"
                                f"{awaiting_viewport[1]}"
                            )
                            await socket.send(
                                json.dumps(
                                    {
                                        "type": "viewport",
                                        "w": awaiting_viewport[0],
                                        "h": awaiting_viewport[1],
                                    }
                                )
                            )
                        elif awaiting_viewport == answered:
                            awaiting_viewport = None
                            if viewports:
                                awaiting_viewport = viewports.pop(0)
                                print(
                                    f"  -> viewport {awaiting_viewport[0]}x"
                                    f"{awaiting_viewport[1]}"
                                )
                                await socket.send(
                                    json.dumps(
                                        {
                                            "type": "viewport",
                                            "w": awaiting_viewport[0],
                                            "h": awaiting_viewport[1],
                                        }
                                    )
                                )
                        if (
                            args.mouse is not None
                            and not pending
                            and not mouse_sent
                            and (
                                args.mouse_width is None
                                or data["w"] == args.mouse_width
                            )
                        ):
                            x, y = args.mouse
                            print(f"  -> mouseMove {x},{y}")
                            await socket.send(
                                json.dumps({"type": "mouseMove", "x": x, "y": y})
                            )
                            mouse_sent = True
                    elif kind == "displays":
                        print(f"  displays  active={data['active']:#x}")
                        for display in data["displays"]:
                            mark = "*" if display["id"] == data["active"] else " "
                            main_display = " (main)" if display["main"] else ""
                            print(
                                f"    {mark} id={display['id']:#x}  "
                                f"{display['label']!r}  {display['detail']!r}"
                                f"{main_display}"
                            )
                        if pending:
                            pick = pending.pop(0)
                            print(f"  -> selectDisplay {pick:#x}")
                            await socket.send(
                                json.dumps({"type": "selectDisplay", "id": pick})
                            )
                        elif args.burst and viewports and not burst_sent:
                            burst_sent = True
                            for viewport in viewports:
                                print(f"  -> viewport {viewport[0]}x{viewport[1]}")
                                await socket.send(
                                    json.dumps(
                                        {
                                            "type": "viewport",
                                            "w": viewport[0],
                                            "h": viewport[1],
                                        }
                                    )
                                )
                            awaiting_viewport = viewports[-1]
                            viewports.clear()
                        elif viewports and awaiting_viewport is None:
                            awaiting_viewport = viewports.pop(0)
                            print(
                                f"  -> viewport {awaiting_viewport[0]}x"
                                f"{awaiting_viewport[1]}"
                            )
                            await socket.send(
                                json.dumps(
                                    {
                                        "type": "viewport",
                                        "w": awaiting_viewport[0],
                                        "h": awaiting_viewport[1],
                                    }
                                )
                            )
                    elif kind == "error":
                        print(f"  !! error: {data['message']}")
                        return 1
                    elif kind == "clipboard":
                        # `requested` is the panel's Fetch being answered; without it
                        # this is the remote having copied something, which is what
                        # drives the browser's automatic sync.
                        how = "fetched" if data["requested"] else "pushed"
                        oversized = data["oversizedBytes"]
                        size = f"  oversizedBytes={oversized}" if oversized else ""
                        print(
                            f"  clipboard ({how})  changedAtMs={data['changedAtMs']}"
                            f"{size}  text={data['text']!r}"
                        )
                    elif kind == "connected":
                        print(
                            f"  connected  {data['name']}  resize={data['resize']}"
                            f"  clipboard={data['clipboard']}"
                        )
                        if args.clipboard is not None and not clipboard_sent:
                            clipboard_sent = True
                            # The fetch first, so what the remote already held is
                            # reported before this end takes the clipboard over.
                            print("  -> clipboardRequest")
                            await socket.send(json.dumps({"type": "clipboardRequest"}))
                            print(f"  -> clipboard {args.clipboard!r}")
                            await socket.send(
                                json.dumps({"type": "clipboard", "text": args.clipboard})
                            )
                    elif kind not in ("cursor", "picker"):
                        print(f"  {kind}: {json.dumps(data)[:120]}")
        except TimeoutError:
            pass
        finally:
            if reconnect_task is not None:
                reconnect_task.cancel()
            if keys_task is not None and not keys_task.done():
                keys_task.cancel()
            if chords_task is not None and not chords_task.done():
                chords_task.cancel()
            if sweep_task is not None and not sweep_task.done():
                sweep_task.cancel()
            if page_task is not None and not page_task.done():
                page_task.cancel()
        print(f"\n  {tiles} tile frames")
        await socket.send(json.dumps({"type": "disconnect"}))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
