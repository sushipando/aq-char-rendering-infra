"""Real encoder/decoder regression: CHAR_RENDER_AVIF_RGBA=/path/to/avif-rgba pytest this_file."""
import io
import json
import os
from pathlib import Path
import random
import struct
import subprocess
import tempfile
import unittest

from PIL import Image


@unittest.skipUnless(os.environ.get("CHAR_RENDER_AVIF_RGBA"), "requires the native AVIF helper")
class AvifRgbaTest(unittest.TestCase):
    def encode(self, frames, durations, *, quality=70, lossless=False):
        width, height = 31, 17  # odd dimensions exercise 4:4:4 and row strides
        header = struct.pack("<8I", width, height, len(frames), quality, 8, lossless, 2, len(frames) > 1)
        data = header + b"".join(struct.pack("<I", duration) + frame for duration, frame in zip(durations, frames))
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "image.avif"
            result = subprocess.run([os.environ["CHAR_RENDER_AVIF_RGBA"], str(output)], input=data, capture_output=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            report = json.loads(result.stdout)
            encoded = output.read_bytes()
        self.assertEqual(report["duration_ms"], sum(durations))
        self.assertEqual(report["physical_frame_count"], len(frames))
        self.assertEqual(report["bytes"], len(encoded))
        image = Image.open(io.BytesIO(encoded))
        self.assertEqual(image.size, (width, height))
        self.assertEqual(image.n_frames, len(frames))
        decoded = []
        for i in range(image.n_frames):
            image.seek(i)
            image.load()
            decoded.append(image.convert("RGBA").tobytes())
            if len(frames) > 1:
                self.assertEqual(round(image.info["duration"]), durations[i])
        return decoded

    def test_lossless_exact_rgba_including_transparent_rgb_and_late_alpha(self):
        rng = random.Random(42)
        frames = [bytes(rng.randrange(256) if n % 4 != 3 else 255 for n in range(31 * 17 * 4)),
                  bytes(rng.randrange(256) for _ in range(31 * 17 * 4)),
                  bytes([255, 0, 250, 0]) * (31 * 17)]
        self.assertEqual(self.encode(frames, [41, 83, 125], lossless=True), frames)
        self.assertEqual(self.encode(frames[:1], [42], lossless=True), frames[:1])
        self.assertEqual(self.encode(frames[:2], [42, 42], quality=100), frames[:2])

    def test_lossy_keeps_alpha_exact_and_constant_animation_keeps_timing(self):
        frames = [bytes((n * 71 + i * 19) % 256 for n in range(31 * 17 * 4)) for i in range(3)]
        for quality in (0, 70):
            decoded = self.encode(frames, [5, 42, 123], quality=quality)
            for before, after in zip(frames, decoded):
                self.assertEqual(before[3::4], after[3::4])
        repeated = [frames[0], frames[0]]
        self.assertEqual(self.encode(repeated, [420, 42], lossless=True), repeated)

    def test_rejects_truncated_input_without_output(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "image.avif"
            data = struct.pack("<9I", 31, 17, 1, 70, 8, 0, 2, 0, 42) + b"short"
            result = subprocess.run([os.environ["CHAR_RENDER_AVIF_RGBA"], str(output)], input=data, capture_output=True, timeout=60)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(output.exists())
