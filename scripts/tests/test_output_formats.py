"""Offline submission and CDN validation for both output formats."""
import sys
import unittest
from email.message import Message
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import submit_render as submit
import smoke_test_deployment as smoke


class OutputFormatTests(unittest.TestCase):
    def test_submission_format_quality_and_shared_lossless_flags(self):
        defaults = submit.parser().parse_args(["Annie"])
        self.assertEqual(defaults.output_format, "webp")
        args = submit.parser().parse_args(["Annie", "--format", "avif", "--avif-quality", "63", "--avif-speed", "8", "--lossless"])
        self.assertEqual((args.output_format, args.avif_quality, args.avif_speed, args.webp_lossless), ("avif", 63, 8, True))

    def test_cdn_checks_format_and_mime_type(self):
        base = "https://example.test"
        for fmt, header in [("avif", b"\x00\x00\x00 ftypavis\x00\x00\x00\x00"), ("webp", b"RIFF\x20\x00\x00\x00WEBPVP8X")]:
            response = MagicMock()
            response.read.return_value = header
            response.status = 200
            response.headers = Message()
            response.headers["Content-Type"] = f"image/{fmt}"
            response.__enter__.return_value = response
            with patch.object(smoke.urllib.request, "urlopen", return_value=response):
                result = smoke.verify_image(f"{base}/renders/result.{fmt}", base, fmt)
                self.assertEqual(result["content_type"], f"image/{fmt}")
                with self.assertRaises(RuntimeError):
                    smoke.verify_image(f"{base}/renders/result.{fmt}", base, "avif" if fmt == "webp" else "webp")
