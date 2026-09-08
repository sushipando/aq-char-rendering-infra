"""Full restart tests; no AWS access and no special workflow entry point."""

import io
import json
import sys
import unittest
from dataclasses import asdict
from pathlib import Path
from unittest.mock import MagicMock, patch

from boto3.dynamodb.types import TypeDeserializer
from botocore.exceptions import ClientError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import restart_render as restart
import submit_render as submit

SOURCE = "eccabdb9-d329-4e3d-a0a8-5b30eb7425df"
MANUAL = "94588f3a-73d4-4357-950b-d4c2305ff86f"
MACHINE = "arn:aws:states:us-west-2:123456789012:stateMachine:test"
ARN = MACHINE.replace(":stateMachine:", ":execution:") + ":" + SOURCE
OUTPUTS = {
    "StateMachineArn": MACHINE,
    "WorkResultBucketName": "work",
    "JobTableName": "jobs",
    "JobQueueUrl": "queue",
}


class RestartTests(unittest.TestCase):
    def setUp(self):
        self.original = restart.JobRequest.from_dict(
            {
                "schema_version": 1,
                "job_id": SOURCE,
                "created_at": "2026-09-05T00:00:00Z",
                "discord": {"user_id": "1", "channel_id": "2", "guild_id": "3"},
                "render": {
                    "username": "Dalvi",
                    "raster_size": 4096,
                    "output_size": 1024,
                    "zoom": 1,
                    "max_frames": 120,
                    "facing": "left",
                    "webp_method": 2,
                    "webp_quality": 75,
                },
                "appearance": {
                    "strName": "Dalvi",
                    "strGender": "F",
                    "strPetFile": "items/pets/dragon.swf",
                    "intColorHair": "10066329",
                },
                "component_raster_mode": "distributed",
                "bounds_mode": "inline",
                "cache": {
                    "render": True,
                    "animation": False,
                    "vectors": True,
                    "bounds": True,
                    "components": True,
                },
            }
        )
        self.jobs, self.sfn, self.s3 = MagicMock(), MagicMock(), MagicMock()
        self.jobs.get.return_value = {"execution_arn": ARN}
        self.set_execution(self.original.to_dict())
        self.args = restart.parser().parse_args([SOURCE])

    def set_execution(self, payload):
        self.sfn.describe_execution.return_value = {
            "input": json.dumps({"request": payload})
        }

    def load(self, source=SOURCE):
        return restart.load_original_request(
            OUTPUTS, source, self.jobs, self.sfn, self.s3
        )

    def saved(self, **changes):
        prepared = {
            "job_id": SOURCE,
            "settings": self.original.render.to_dict(),
            "fields": self.original.appearance,
            **changes,
        }
        return {"Body": io.BytesIO(json.dumps(prepared).encode())}

    def test_only_job_id_and_creation_time_change_by_default(self):
        original, _ = self.load()
        fresh = restart.fresh_request(original, self.args)
        self.assertNotEqual(fresh.job_id, original.job_id)
        self.assertNotEqual(fresh.created_at, original.created_at)
        before, after = original.to_dict(), fresh.to_dict()
        self.assertEqual(
            {key for key in before if before[key] != after[key]},
            {"job_id", "created_at", "appearance"},
        )
        self.assertIsNone(fresh.appearance)
        self.assertEqual(fresh.source_job_id, original.job_id)
        self.assertEqual(original.appearance["intColorHair"], "10066329")
        self.s3.get_object.assert_not_called()

    def test_cache_overrides_are_explicit_and_independent(self):
        for field, flag in restart.CACHE_FLAGS.items():
            args = restart.parser().parse_args([SOURCE, f"--no-{flag}-cache"])
            fresh = restart.fresh_request(self.original, args)
            expected = {**asdict(self.original.cache), field: False}
            self.assertEqual(asdict(fresh.cache), expected)
            self.assertEqual(fresh.render, self.original.render)
        args = restart.parser().parse_args([SOURCE, "--no-cache"])
        self.assertFalse(
            any(asdict(restart.fresh_request(self.original, args).cache).values())
        )

    def test_execution_input_wins_over_sparse_admission_record(self):
        self.jobs.get.return_value["request"] = {"render": {"username": "Dalvi"}}
        original, _ = self.load()
        self.assertEqual(original, self.original)

    def test_does_not_require_source_terminal_status_or_slot_release(self):
        for status in ["FAILED", "SUCCEEDED", "PREPARING"]:
            self.jobs.get.return_value.update(status=status, slot_released=False)
            self.assertEqual(self.load()[0], self.original)

    def test_console_execution_name_can_differ_from_embedded_job_id(self):
        self.jobs.get.return_value = None
        request, arn = self.load(MANUAL)
        self.assertEqual(request.job_id, SOURCE)
        self.assertTrue(arn.endswith(MANUAL))
        self.assertNotIn(
            restart.fresh_request(request, self.args).job_id, (SOURCE, MANUAL)
        )

    def test_accepts_same_machine_arn_and_rejects_other_machine(self):
        self.assertEqual(self.load(ARN)[0], self.original)
        self.jobs.get.assert_not_called()
        with self.assertRaisesRegex(ValueError, "configured render state machine"):
            self.load(ARN.replace(":execution:test:", ":execution:other:"))

    def test_missing_appearance_is_resolved_in_aws_not_on_cli(self):
        payload = self.original.to_dict()
        payload["appearance"] = None
        self.set_execution(payload)
        original, _ = self.load()
        fresh = restart.fresh_request(original, self.args)
        self.assertIsNone(fresh.appearance)
        self.assertEqual(fresh.source_job_id, SOURCE)
        self.s3.get_object.assert_not_called()

    def test_shared_enqueue_creates_new_admission_then_queues_identical_request(self):
        fresh = restart.fresh_request(self.original, self.args)
        sqs = MagicMock()
        with (
            patch.object(submit, "JobStore", return_value=self.jobs),
            patch.object(submit.boto3, "client", return_value=sqs),
        ):
            job, payload = restart.enqueue_request(OUTPUTS, fresh, 2)
        self.assertEqual(job, fresh.job_id)
        self.jobs.acquire.assert_called_once_with(fresh, 2)
        self.assertEqual(
            json.loads(sqs.send_message.call_args.kwargs["MessageBody"]), payload
        )
        self.assertEqual(payload, fresh.to_dict())
        self.assertNotIn("resume", payload)
        self.jobs.mark_execution.assert_not_called()
        self.jobs.release.assert_not_called()
        self.sfn.start_execution.assert_not_called()
        self.sfn.redrive_execution.assert_not_called()

    def test_restart_of_discord_job_is_admitted_as_cli_without_changing_inputs(self):
        self.jobs.get.return_value["request_origin"] = "discord"
        original, _ = self.load()
        fresh = restart.fresh_request(original, self.args)
        dynamodb, sqs = MagicMock(), MagicMock()
        with patch.object(
            submit.boto3, "client",
            side_effect=lambda name: {"dynamodb": dynamodb, "sqs": sqs}[name],
        ):
            restart.enqueue_request(OUTPUTS, fresh, 2)
        raw = dynamodb.transact_write_items.call_args.kwargs["TransactItems"][0]["Put"]["Item"]
        admitted = {key: TypeDeserializer().deserialize(value) for key, value in raw.items()}
        self.assertEqual(admitted["request_origin"], "cli")
        self.assertEqual(admitted["user_id"], original.discord.user_id)
        self.assertEqual(admitted["request"], fresh.to_dict())
        self.assertEqual(
            json.loads(sqs.send_message.call_args.kwargs["MessageBody"]), fresh.to_dict()
        )

    def test_enqueue_failure_releases_only_new_admission(self):
        fresh = restart.fresh_request(self.original, self.args)
        sqs = MagicMock()
        sqs.send_message.side_effect = RuntimeError("send failed")
        with (
            patch.object(submit, "JobStore", return_value=self.jobs),
            patch.object(submit.boto3, "client", return_value=sqs),
            self.assertRaisesRegex(RuntimeError, "send failed"),
        ):
            restart.enqueue_request(OUTPUTS, fresh, 2)
        self.assertEqual(self.jobs.release.call_args.args[:2], (fresh.job_id, "FAILED"))

    def test_dry_run_never_submits_or_admits(self):
        with (
            patch.object(sys, "argv", ["restart_render.py", SOURCE, "--dry-run"]),
            patch.object(restart, "load_outputs", return_value=OUTPUTS),
            patch.object(restart, "JobStore", return_value=self.jobs),
            patch.object(restart.boto3, "client", side_effect=[self.sfn, self.s3]),
            patch.object(restart, "enqueue_request") as enqueue,
            patch("builtins.print"),
        ):
            self.assertEqual(restart.main(), 0)
        enqueue.assert_not_called()
        self.jobs.acquire.assert_not_called()


if __name__ == "__main__":
    unittest.main()
