"""Exercise the smoke command offline: no Discord queue delivery is required."""

import io
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from boto3.dynamodb.types import TypeDeserializer, TypeSerializer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import smoke_test_deployment as smoke


class SmokeNotificationTests(unittest.TestCase):
    def test_cli_smoke_uses_persisted_result_without_discord_publication(self):
        for status in ("SUCCEEDED", "FAILED"):
            with self.subTest(status=status):
                dynamodb, sqs, ssm = MagicMock(), MagicMock(), MagicMock()
                ssm.get_parameter.return_value = {"Parameter": {"Value": "true"}}
                result = {"url": "https://example.test/renders/test.webp", "width": 256, "height": 128}
                terminal = {
                    "status": status,
                    "result_payload": {"status": status, "result": result},
                    # Intentionally no result_enqueued_at: CLI completion is silent.
                }
                dynamodb.get_item.return_value = {
                    "Item": {key: TypeSerializer().serialize(value) for key, value in terminal.items()}
                }
                outputs = {
                    "JobTableName": "jobs", "JobQueueUrl": "jobs-queue",
                    "RenderEnabledParameterName": "enabled", "CloudFrontBaseUrl": "https://example.test",
                }
                with (
                    patch.object(sys, "argv", ["smoke_test_deployment.py", "--username", "Annie"]),
                    patch.object(smoke, "load_outputs", return_value=outputs),
                    patch.object(smoke.boto3, "client", side_effect={
                        "dynamodb": dynamodb, "sqs": sqs, "ssm": ssm,
                    }.__getitem__),
                    patch.object(smoke.tryon, "fetch_character_flashvars", side_effect=AssertionError("CLI must not fetch AQW")),
                    patch.object(smoke, "seed_missing_assets", side_effect=AssertionError("CLI must not seed sources")),
                    patch.object(smoke, "verify_webp", return_value={}) as verify,
                    patch.object(sys, "stdout", io.StringIO()),
                ):
                    if status == "SUCCEEDED":
                        self.assertEqual(smoke.main(), 0)
                        verify.assert_called_once_with(result["url"], outputs["CloudFrontBaseUrl"])
                    else:
                        with self.assertRaisesRegex(RuntimeError, "Render failed"):
                            smoke.main()
                        verify.assert_not_called()
                raw = dynamodb.transact_write_items.call_args.kwargs["TransactItems"][0]["Put"]["Item"]
                admitted = {key: TypeDeserializer().deserialize(value) for key, value in raw.items()}
                self.assertEqual(admitted["request_origin"], "cli")
                sqs.send_message.assert_called_once()
                self.assertEqual(sqs.send_message.call_args.kwargs["QueueUrl"], "jobs-queue")
                sqs.receive_message.assert_not_called()
                sqs.delete_message.assert_not_called()


if __name__ == "__main__":
    unittest.main()
