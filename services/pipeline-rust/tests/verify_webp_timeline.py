"""Independent libwebp/Pillow decoding check used by opt-in Rust mux tests.

Only reads images; compares RGBA over every interval in the union of both
timelines, including run splits. Memory is bounded to the current decoded frame.
"""

import hashlib
import json
import sys

from PIL import Image


def timeline(path):
    frames = []
    timestamp = 0
    with Image.open(path) as image:
        size = image.size
        loop = image.info.get("loop")
        for index in range(image.n_frames):
            image.seek(index)
            image.load()
            duration = image.info.get("duration")
            digest = hashlib.sha256(image.convert("RGBA").tobytes()).hexdigest()
            frames.append((timestamp, timestamp + (duration or 0), digest))
            timestamp += duration or 0
    return size, loop, frames


def verify(reference, candidate):
    size, loop, before = timeline(reference)
    new_size, new_loop, after = timeline(candidate)
    assert size == new_size, "canvas changed"
    assert loop == new_loop, "loop metadata changed"
    assert before[-1][1] == after[-1][1], "total duration changed"
    if before[-1][1] == 0:  # A genuine single-image request has no ANMF timing.
        assert len(before) == len(after) == 1 and before[0][2] == after[0][2]
    else:
        i = j = 0
        while i < len(before) and j < len(after):
            start = max(before[i][0], after[j][0])
            end = min(before[i][1], after[j][1])
            assert end > start, "invalid interval"
            assert before[i][2] == after[j][2], f"RGBA differs at {start}..{end}ms"
            if before[i][1] == end:
                i += 1
            if after[j][1] == end:
                j += 1
        assert i == len(before) and j == len(after), "incomplete timeline"
    return {
        "decoded_rgba_timeline_exact": True,
        "before_frames": len(before),
        "after_frames": len(after),
        "duration_ms": before[-1][1],
        "canvas": size,
    }


if __name__ == "__main__":
    print(json.dumps(verify(sys.argv[1], sys.argv[2])))
