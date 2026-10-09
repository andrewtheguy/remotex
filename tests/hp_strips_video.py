#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy"]
# ///
"""A High Performance Mac's capture in four strips, as a video of the display to watch.

``tests/hp_capture.sh --strips`` writes ``video.h265`` with each picture one strip of
the display, and nothing in it saying which. A player shows the strips one after
another; this puts each where it belongs and writes the display as it stood after every
picture, as H.264:

    uv run tests/hp_strips_video.py tmp/hp-capture/video.h265 tmp/hp-capture/display.mp4 \
        --rows 900 --seconds 45

``--rows`` is the display's height: the strips are a quarter of it rounded up to a
multiple of 16, so the last runs past the display's last row and what it holds there is
not picture. ``--seconds`` is how long the capture ran, which sets the frame rate: the
dump carries no timestamps, and the pictures are spread evenly over it.

A picture's strip is read from its slice header (ffmpeg's ``trace_headers``): it names
the pictures it predicts from, and the oldest of those it uses is an earlier picture of
its own strip. A keyframe is an IDR of strip 0 followed by a picture of each other
strip in turn. Needs ``ffmpeg`` and ``ffprobe`` on ``PATH``.
"""

import argparse
import re
import subprocess
import sys

import numpy as np

STRIPS = 4
FIELD = re.compile(r"\] \d+\s+(\w+)(?:\[\d+\])?\s+[01]+ = (-?\d+)\s*$")


def reference_sets(src: str) -> list[dict]:
    """Each picture's NAL type and short-term reference set, in decoding order."""
    trace = subprocess.run(
        ["ffmpeg", "-v", "trace", "-i", src, "-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"],
        capture_output=True,
        text=True,
        errors="replace",
        check=True,
    ).stderr
    slices: list[dict] = []
    sps: list[tuple[list[int], list[int]]] = []
    cur = None
    in_sps = False
    for line in trace.split("\n"):
        if "Sequence Parameter Set" in line:
            sps, in_sps, cur = [], True, None
            continue
        if "Slice Segment Header" in line:
            cur, in_sps = {"type": None, "deltas": [], "used": []}, False
            slices.append(cur)
            continue
        m = FIELD.search(line)
        if not m:
            continue
        name, value = m.group(1), int(m.group(2))
        if name == "inter_ref_pic_set_prediction_flag" and value == 1:
            sys.exit("a reference set predicted from another, which this does not read")
        if in_sps:
            if name == "num_negative_pics":
                sps.append(([], []))
            elif name == "delta_poc_s0_minus1":
                sps[-1][0].append(value + 1)
            elif name == "used_by_curr_pic_s0_flag":
                sps[-1][1].append(value)
            elif name == "nal_unit_type" and value != 33:
                in_sps = False
            continue
        if cur is None:
            continue
        if name == "nal_unit_type" and cur["type"] is None:
            cur["type"] = value
        elif name == "delta_poc_s0_minus1":
            cur["deltas"].append(value + 1)
        elif name == "used_by_curr_pic_s0_flag":
            cur["used"].append(value)
        elif name == "short_term_ref_pic_set_idx":
            cur["deltas"], cur["used"] = list(sps[value][0]), list(sps[value][1])
        elif name == "slice_qp_delta":
            # The header's last field read here; a parameter set's follow it.
            cur = None
    return slices


def strips_of(slices: list[dict]) -> list[int]:
    """The strip each picture is. A picture's order count is its place after the IDR."""
    strips: list[int] = []
    by_poc: dict[int, int] = {}
    poc = 0
    for s in slices:
        if s["type"] in (19, 20):
            poc, by_poc, strip = 0, {}, 0
        else:
            poc += 1
            at, known = poc, []
            for delta, used in zip(s["deltas"], s["used"]):
                at -= delta
                if used and at in by_poc:
                    known.append(by_poc[at])
            if poc < STRIPS:
                strip = poc
            elif known:
                strip = known[-1]
            else:
                sys.exit(f"picture {len(strips)} uses no picture whose strip is known")
        by_poc[poc] = strip
        strips.append(strip)
    return strips


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("src", help="the capture, video.h265")
    parser.add_argument("dst", help="the video to write, by its extension")
    parser.add_argument("--rows", type=int, required=True, help="the display's height")
    parser.add_argument("--seconds", type=float, required=True, help="how long the capture ran")
    args = parser.parse_args()

    strips = strips_of(reference_sets(args.src))
    size = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "stream=width,height", "-of", "csv=p=0", args.src],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split(",")
    w, h = int(size[0]), int(size[1])
    if not (STRIPS - 1) * h < args.rows <= STRIPS * h:
        sys.exit(f"--rows {args.rows} is not a display of four strips {h} rows high")
    decoder = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-i", args.src, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"],
        stdout=subprocess.PIPE,
    )
    encoder = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{w}x{args.rows}"]
        + ["-r", f"{len(strips) / args.seconds:.3f}", "-i", "-"]
        + ["-c:v", "libx264", "-crf", "18", "-pix_fmt", "yuv420p", args.dst],
        stdin=subprocess.PIPE,
    )
    display = np.zeros((STRIPS * h, w, 3), np.uint8)
    placed = 0
    for strip in strips:
        picture = decoder.stdout.read(w * h * 3)
        if len(picture) < w * h * 3:
            break
        display[strip * h : (strip + 1) * h] = np.frombuffer(picture, np.uint8).reshape(h, w, 3)
        encoder.stdin.write(display[: args.rows].tobytes())
        placed += 1
    encoder.stdin.close()
    if encoder.wait() != 0 or decoder.wait() != 0 or placed != len(strips):
        sys.exit(f"{placed} of {len(strips)} pictures written; ffmpeg failed")
    counts = [strips.count(k) for k in range(STRIPS)]
    print(f"{placed} pictures, of strips 0 to 3: {counts}; {args.dst}")


if __name__ == "__main__":
    main()
