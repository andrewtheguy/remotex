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
import os
import sys

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
        "--select",
        type=lambda value: int(value, 0),
        action="append",
        default=[],
        help="display id to select once the list arrives (repeatable)",
    )
    parser.add_argument("--mouse", type=coordinates, default=None)
    parser.add_argument("--mouse-width", type=int, default=None)
    parser.add_argument(
        "--viewport",
        type=dimensions,
        action="append",
        default=[],
        help="viewport WIDTHxHEIGHT to request after the display list arrives (repeatable)",
    )
    parser.add_argument(
        "--burst",
        action="store_true",
        help="send all viewport requests together instead of waiting for each answer",
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

    url = f"ws://127.0.0.1:{args.port}/ws?session={token}"
    async with websockets.connect(
        url, additional_headers={"Cookie": f"remotex_session={cookie}"}
    ) as socket:
        await socket.send(json.dumps({"type": "connect", "target": args.target}))
        pending = list(args.select)
        viewports = list(args.viewport)
        awaiting_viewport = None
        burst_sent = False
        mouse_sent = False
        tiles = 0
        try:
            async with asyncio.timeout(args.seconds):
                async for message in socket:
                    if isinstance(message, bytes):
                        tiles += 1
                        continue
                    data = json.loads(message)
                    kind = data.get("type")
                    if kind == "resize":
                        print(
                            f"  resize  {data['w']}x{data['h']}  scale={data['scale']}"
                            f"   -> {data['w'] / data['scale']:g}x"
                            f"{data['h'] / data['scale']:g} CSS px"
                        )
                        if awaiting_viewport == (data["w"], data["h"]):
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
                    elif kind == "connected":
                        print(f"  connected  resize={data['resize']}")
                    elif kind not in ("cursor", "picker"):
                        print(f"  {kind}: {json.dumps(data)[:120]}")
        except TimeoutError:
            pass
        print(f"\n  {tiles} tile frames")
        await socket.send(json.dumps({"type": "disconnect"}))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
