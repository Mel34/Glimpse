#!/usr/bin/env python3
"""Compare an RGBA GPU framebuffer dump against an RGB24 FFmpeg reference."""

import argparse
from pathlib import Path


def compare(label, actual, reference, width, height):
    channel_sums = [0, 0, 0]
    channel_maxima = [0, 0, 0]
    histogram = [0] * 256
    total = 0

    for offset, (a, b) in enumerate(zip(actual, reference)):
        difference = abs(a - b)
        channel = offset % 3
        channel_sums[channel] += difference
        channel_maxima[channel] = max(
            channel_maxima[channel], difference
        )
        histogram[difference] += 1
        total += difference

    sample_count = width * height * 3

    def percentile(percent):
        target = int(sample_count * percent / 100)
        cumulative = 0
        for value, count in enumerate(histogram):
            cumulative += count
            if cumulative > target:
                return value
        return 255

    print(f"\n{label}")
    print(f"  Overall MAE: {total / sample_count:.4f}")
    for channel, name in enumerate(("R", "G", "B")):
        print(
            f"  {name}: MAE={channel_sums[channel] / (width * height):.4f}, "
            f"max={channel_maxima[channel]}"
        )
    print(f"  P95: {percentile(95)}")
    print(f"  P99: {percentile(99)}")
    for threshold in (2, 5, 10, 20, 40):
        count = sum(histogram[threshold + 1:])
        print(
            f"  Differences > {threshold:2}: "
            f"{count / sample_count * 100:.4f}%"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gpu_rgba", type=Path)
    parser.add_argument("reference_rgb24", type=Path)
    parser.add_argument("--width", type=int, default=720)
    parser.add_argument("--height", type=int, default=720)
    args = parser.parse_args()

    if args.width <= 0 or args.height <= 0:
        parser.error("width and height must be positive")

    gpu = args.gpu_rgba.read_bytes()
    reference = args.reference_rgb24.read_bytes()
    pixels = args.width * args.height

    if len(gpu) != pixels * 4:
        parser.error(
            f"{args.gpu_rgba}: expected {pixels * 4} bytes, got {len(gpu)}"
        )
    if len(reference) != pixels * 3:
        parser.error(
            f"{args.reference_rgb24}: expected {pixels * 3} bytes, "
            f"got {len(reference)}"
        )

    rows = []
    for y in range(args.height):
        source = y * args.width * 4
        row = bytearray(args.width * 3)
        for x in range(args.width):
            src = source + x * 4
            dst = x * 3
            row[dst:dst + 3] = gpu[src:src + 3]
        rows.append(bytes(row))

    # OpenGL framebuffer readback is conventionally bottom-up. Compare both
    # orientations rather than assuming a vertical flip is required.
    compare(
        "GPU capture as read (bottom-up)",
        b"".join(rows),
        reference,
        args.width,
        args.height,
    )
    compare(
        "GPU capture vertically flipped (top-down)",
        b"".join(reversed(rows)),
        reference,
        args.width,
        args.height,
    )


if __name__ == "__main__":
    main()
