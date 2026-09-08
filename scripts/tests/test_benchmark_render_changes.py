import json
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import benchmark_render_changes as bench
from aqw_char_renderer.contracts import JobRequest


def request(job="8d1c70fd-6c7a-4abc-a539-014575b09078"):
    return JobRequest.from_dict({"schema_version":1,"job_id":job,"created_at":"2026-09-06T00:00:00Z",
        "discord":{"user_id":"1","channel_id":"2"},
        "render":{"username":"Annie","raster_size":4096,"output_size":2048},
        "appearance":{"strName":"Annie","strCapeFile":"frozen.swf"},
        "cache":{"render":False,"animation":True,"vectors":True,"bounds":False,"components":False}})


class BenchmarkTests(unittest.TestCase):
    def test_log_discovery_uses_rendered_id_not_restart_parent(self):
        with tempfile.TemporaryDirectory() as root:
            path=Path(root)/"annie-1.log"
            path.write_text(json.dumps({"job_id":request().job_id,"restarted_from_job_id":bench.REFERENCE})+'\n status -> SUCCEEDED\n'+json.dumps({"job_id":request().job_id,"status":"SUCCEEDED"}))
            jobs=bench.discover(Path(root),[bench.REFERENCE])
            self.assertEqual(len(jobs),2)
            self.assertEqual(jobs[request().job_id],"annie-1")

    def test_repeats_group_by_frozen_inputs_not_name(self):
        a=request(); b=replace(a,job_id=bench.REFERENCE,created_at="2026-09-06T01:00:00Z")
        c=replace(b,appearance={"strName":"Annie","strCapeFile":"different.swf"})
        groups=bench.group_baselines([{"request":x.to_dict()} for x in [a,b,c]])
        self.assertEqual(sorted(len(g["baselines"]) for g in groups),[1,2])
        self.assertNotEqual(bench.fingerprint(a),bench.fingerprint(replace(a,cache=replace(a.cache,vectors=False))))

    def test_variants_preserve_appearance_resolution_and_cache_policy(self):
        original=request()
        for mode in ("matched","export-cold","avif-raw","avif-zstd","avif-lossless"):
            candidate=bench.candidate(original,mode,70,8)
            self.assertIsNone(candidate.appearance)
            self.assertEqual(candidate.source_job_id,original.job_id)
            self.assertEqual(candidate.render.output_size,2048)
            self.assertEqual(candidate.render.raster_size,4096)
            self.assertFalse(candidate.cache.render)
            self.assertNotEqual(candidate.job_id,original.job_id)
            self.assertEqual(candidate.cache.animation,original.cache.animation)
            if mode=="matched": self.assertEqual(candidate.render,original.render)
            if mode=="export-cold": self.assertFalse(candidate.cache.vectors)
            if mode=="avif-raw": self.assertEqual(candidate.render.rgba_compression,"none")
            if mode=="avif-zstd": self.assertEqual(candidate.render.rgba_compression,"zstd")

    def test_parallel_identically_named_states_follow_event_ancestry(self):
        events=[
            {"id":1,"type":"TaskStateEntered","timestamp":"2026-09-06T00:00:00Z","stateEnteredEventDetails":{"name":"Raster"}},
            {"id":2,"type":"TaskStateEntered","timestamp":"2026-09-06T00:00:02Z","stateEnteredEventDetails":{"name":"Raster"}},
            {"id":3,"previousEventId":1,"type":"TaskStateExited","timestamp":"2026-09-06T00:00:10Z","stateExitedEventDetails":{"name":"Raster"}},
            {"id":4,"previousEventId":2,"type":"TaskStateExited","timestamp":"2026-09-06T00:00:05Z","stateExitedEventDetails":{"name":"Raster"}},
        ]
        stats=bench.state_times(events)["Raster"]
        self.assertEqual((stats["min_s"],stats["max_s"],stats["count"]),(3,10,2))

    def test_result_shapes_and_changed_work_warning(self):
        result={"final_key":"renders/image.webp","bytes":123,"frame_count":120}
        self.assertEqual(bench.find_result([result]),result)
        self.assertEqual(bench.find_result({"result":{"Payload":result}}),result)
        group={"baselines":[{"summary":{"result":result}}]}
        self.assertIn("frame_count differs from historical output",bench.comparison_warnings(group,{"result":{**result,"frame_count":60}}))

    def test_aliases_and_encoding_settings_are_not_pooled(self):
        original = request()
        groups = bench.group_baselines([{"request": original.to_dict(), "summary": {"label": "annie-3"}}])
        self.assertEqual(groups[0]["aliases"], ["annie"])
        rows = [{"variant": "avif-zstd", "request": bench.candidate(original, "avif-zstd", q, 8).to_dict()} for q in (70, 85)]
        self.assertNotEqual(bench.variant_label(rows[0]), bench.variant_label(rows[1]))

    def test_capture_never_submits_and_completed_run_resumes_without_enqueue(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            summary = {"job_id": request().job_id, "status": "SUCCEEDED", "execution_seconds": 10, "result": {"bytes": 1024}}
            baselines = [{"request": request().to_dict(), "summary": summary}]
            bench.write(root / "baselines.json", {"sources": {bench.REFERENCE: "reference"}, "state_machine": "machine", "baselines": baselines})
            evidence = MagicMock()
            evidence.jobs.get.return_value = {"status": "SUCCEEDED"}
            evidence.collect.return_value = summary
            argv = ["benchmark", "--output-dir", str(root), "--checks-dir", str(root / "missing"), "--rounds", "1"]
            with (patch.object(bench.boto3, "setup_default_session"),
                  patch.object(bench, "load_outputs", return_value={"StateMachineArn": "machine"}),
                  patch.object(bench, "Evidence", return_value=evidence),
                  patch.object(bench, "enqueue_request") as enqueue,
                  patch.object(sys, "argv", argv), patch("builtins.print")):
                self.assertEqual(bench.main(), 0)
                enqueue.assert_not_called()
                evidence.jobs.get.assert_not_called()
                argv.append("--run")
                self.assertEqual(bench.main(), 0)
                self.assertEqual(bench.main(), 0)
                enqueue.assert_not_called()
                self.assertEqual(evidence.collect.call_count, 1)

    def test_default_only_captures_and_comparison_can_be_written_offline(self):
        self.assertFalse(bench.parser().parse_args([]).run)
        with tempfile.TemporaryDirectory() as root:
            summary={"job_id":request().job_id,"status":"SUCCEEDED","execution_seconds":10,"result":{"bytes":1024}}
            groups=bench.group_baselines([{"request":request().to_dict(),"summary":summary}])
            bench.report(Path(root),groups)
            self.assertIn("post-IIR baseline",(Path(root)/"REPORT.md").read_text())


if __name__=="__main__": unittest.main()
