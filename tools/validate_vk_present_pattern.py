#!/usr/bin/env python3
"""Check focused, unscaled SDL captures of --present-pattern.

Pass only captures where the probe covers the full image. Desktop/loading
captures are errors, not silently accepted or dropped. This checks sampled
pixels and serial order; it does not prove GPU lifetime or benchmark safety.
Requires Pillow. No image is modified.
"""
import argparse
import json
from pathlib import Path

from PIL import Image


def inspect(path):
    with Image.open(path) as source:
        image = source.convert("RGB")
    width, height = image.size
    if width < 16 or height < 20:
        return {"path": str(path), "error": "capture too small"}
    serial = 0
    for bit in range(16):
        rgb = image.getpixel((width * (2 * bit + 1) // 32, height - 4))
        if all(v <= 12 for v in rgb):
            continue
        if not all(v >= 243 for v in rgb):
            return {"path": str(path), "error": "serial is not black/white", "bit": bit, "rgb": rgb}
        serial |= 1 << bit

    mismatches = []
    samples = 0
    for y in range(12):
        top, bottom = (height - 8) * y // 12, (height - 8) * (y + 1) // 12
        for x in range(16):
            left, right = width * x // 16, width * (x + 1) // 16
            bits = (((x + 1) * 0x45D9F3B) ^ ((y + 7) * 0x27D4EB2D) ^ serial) & 0xFFFFFFFF
            bits ^= bits >> 16
            expected = tuple(255 if bits & (1 << c) else 0 for c in range(3))
            for fy in (1, 2, 3):
                for fx in (1, 2, 3):
                    point = (left + (right - left) * fx // 4, top + (bottom - top) * fy // 4)
                    actual = image.getpixel(point)
                    samples += 1
                    if any(abs(a - b) > 12 for a, b in zip(actual, expected)):
                        mismatches.append({"point": point, "actual": actual, "expected": expected})
    return {"path": str(path), "serial": serial, "samples": samples,
            "mismatches": len(mismatches), "examples": mismatches[:8]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", nargs="+", type=Path)
    args = parser.parse_args()
    failed = False
    previous = None
    for path in args.captures:
        result = inspect(path)
        if "serial" in result:
            result["rollback"] = previous is not None and result["serial"] < previous
            previous = result["serial"]
        failed |= bool(result.get("error") or result.get("mismatches") or result.get("rollback"))
        print(json.dumps(result))
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
