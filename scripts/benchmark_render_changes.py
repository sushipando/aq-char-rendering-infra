"""Operator-run comparison against frozen post-IIR jobs. Never builds or deploys.

Without --run, capture historical evidence and print the replay plan only.
With --run, submit fresh CLI jobs sequentially through normal admission.
"""
from __future__ import annotations

import argparse
import copy
import gzip
import hashlib
import json
import statistics
import re
import time
import shutil
from collections import defaultdict
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path
from uuid import UUID, uuid4

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError
from aqw_char_renderer.contracts import JobRequest, utc_now
from aqw_char_renderer.jobs import JobStore, TERMINAL_STATUSES
from restart_render import load_original_request
from smoke_test_deployment import load_outputs
from submit_render import enqueue_request, wait_for

REFERENCE = "6fd4e305-3ff1-4c5f-a253-65b7e5530242"
CHECKS = Path("/private/tmp/aqw-iir-render-checks-km9USb")
SUITES = {"matched": ["matched"], "exports": ["export-cold"],
          "codecs": ["avif-raw", "avif-zstd"],
          "all": ["matched", "export-cold", "avif-raw", "avif-zstd"]}


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--outputs", type=Path, default=Path("cdk-outputs.dev.json"))
    p.add_argument("--checks-dir", type=Path, default=CHECKS)
    p.add_argument("--job", action="append", help="Additional baseline job alongside the requested ordinaryboy job")
    p.add_argument("--output-dir", type=Path, default=Path("tmp/render-change-benchmark"))
    p.add_argument("--profile", default="aqw-char-dev")
    p.add_argument("--region", default="us-west-2")
    p.add_argument("--run", action="store_true", help="Submit the displayed plan; otherwise only read historical data")
    p.add_argument("--suite", choices=SUITES, default="matched")
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--only", help="Comma-separated workload names, e.g. annie,dalvi,ordinaryboy")
    p.add_argument("--include-lossless", action="store_true", help="Also benchmark AVIF lossless with zstd")
    p.add_argument("--avif-quality", type=int, default=70)
    p.add_argument("--avif-speed", type=int, default=8)
    p.add_argument("--logs", action="store_true", help="Also collect per-job Lambda JSON profiles (extra read-only API calls)")
    p.add_argument("--timeout", type=int, default=1800)
    p.add_argument("--poll", type=float, default=5)
    return p


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True, default=str) + "\n")
    temporary.replace(path)


def discover(directory, jobs):
    if not directory.is_dir():
        raise ValueError(f"Missing baseline directory: {directory}. Pass --checks-dir with its actual location.")
    result = {str(UUID(job)): "reference" for job in jobs}
    paths = sorted(directory.glob("*.log"))
    if not paths:
        raise ValueError(f"No render logs in {directory}")
    for path in paths:
        ids = set()
        for line in path.read_text().splitlines():
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(row, dict) and row.get("job_id"):
                ids.add(str(UUID(row["job_id"])))
        if len(ids) != 1:
            raise ValueError(f"Expected exactly one rendered job in {path}, found {len(ids)}")
        result[ids.pop()] = path.stem
    return result


def stamp(value):
    return value.timestamp() if isinstance(value, datetime) else datetime.fromisoformat(value).timestamp()


def state_times(events):
    """Follow event ancestry, not a name-keyed stopwatch across parallel tasks."""
    by_id = {e["id"]: e for e in events}
    times = defaultdict(list)
    for event in events:
        if not event["type"].endswith("StateExited"):
            continue
        name = event.get("stateExitedEventDetails", {}).get("name")
        parent = event.get("previousEventId")
        seen = set()
        while parent and parent not in seen and parent in by_id:
            seen.add(parent)
            start = by_id[parent]
            if start.get("stateEnteredEventDetails", {}).get("name") == name:
                times[name].append(stamp(event["timestamp"]) - stamp(start["timestamp"]))
                break
            parent = start.get("previousEventId")
    return {k: {"count": len(v), "min_s": min(v), "median_s": statistics.median(v),
                "max_s": max(v), "sum_s": sum(v)} for k, v in times.items()}


def find_result(value):
    if isinstance(value, dict):
        if "final_key" in value and "bytes" in value:
            return value
        for key in ("result", "Payload", "result_payload"):
            found = find_result(value.get(key))
            if found:
                return found
    if isinstance(value, list):
        for child in value:
            found = find_result(child)
            if found:
                return found
    return {}


def fingerprint(request):
    value = request.to_dict()
    # Group repeated renders, irrespective of job identity or Discord destination.
    value = {k: v for k, v in value.items() if k not in {"job_id", "created_at", "discord"}}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def group_baselines(baselines):
    groups = {}
    for baseline in baselines:
        request = JobRequest.from_dict(baseline["request"])
        key = fingerprint(request)
        if key not in groups:
            groups[key] = {"id": key[:12], "name": request.render.username.casefold(),
                           "request": request.to_dict(), "baselines": [], "aliases": []}
        groups[key]["baselines"].append(baseline)
        label = baseline.get("summary", {}).get("label", "")
        alias = re.sub(r"-\d+$", "", label).casefold()
        if alias and alias != "reference" and alias not in groups[key]["aliases"]:
            groups[key]["aliases"].append(alias)
    return sorted(groups.values(), key=lambda g: (g["name"], g["id"]))


def candidate(original, variant, quality, speed):
    request = copy.deepcopy(original)
    cache = replace(request.cache, render=False)
    render = request.render
    if variant == "export-cold":
        cache = replace(cache, vectors=False, bounds=False, components=False)
    elif variant.startswith("avif-"):
        render = replace(render, output_format="avif", avif_quality=quality, avif_speed=speed,
                         webp_lossless=variant == "avif-lossless",
                         rgba_compression="none" if variant == "avif-raw" else "zstd")
    elif variant != "matched":
        raise ValueError(f"Unknown variant {variant}")
    return replace(request, job_id=str(uuid4()), created_at=utc_now(), render=render, cache=cache,
                   appearance=None, source_job_id=request.job_id, appearance_overrides=None)


class Evidence:
    def __init__(self, outputs, root, collect_logs=False):
        self.outputs, self.root, self.collect_logs = outputs, root, collect_logs
        config = Config(connect_timeout=5, read_timeout=30, retries={"mode":"standard", "max_attempts":3})
        self.sfn = boto3.client("stepfunctions", config=config)
        self.s3 = boto3.client("s3", config=config)
        self.jobs = JobStore(outputs["JobTableName"], boto3.client("dynamodb", config=config))
        self.logs = boto3.client("logs", config=config)
        self.lambdas = boto3.client("lambda", config=config)
        self.prefix = outputs["StateMachineArn"].replace(":stateMachine:", ":execution:") + ":"

    def collect(self, job_id, label, wait_running=False):
        folder = self.root / "evidence" / job_id
        summary_path = folder / "summary.json"
        if summary_path.exists():
            if self.collect_logs and not (folder / "profiles.json").exists():
                self.profiles(job_id, json.loads((folder / "execution.json").read_text()), folder)
            return json.loads(summary_path.read_text())
        record = self.jobs.get(job_id) or {}
        arn = record.get("execution_arn") or self.prefix + job_id
        if not arn.startswith(self.prefix):
            raise ValueError("Job belongs to another state machine")
        execution = self.sfn.describe_execution(executionArn=arn)
        deadline = time.monotonic() + 60
        while wait_running and execution["status"] == "RUNNING" and time.monotonic() < deadline:
            time.sleep(2)
            execution = self.sfn.describe_execution(executionArn=arn)
        if execution["status"] == "RUNNING":
            raise ValueError(f"{job_id} is still running")
        write(folder / "execution.json", execution)
        events = []
        for page in self.sfn.get_paginator("get_execution_history").paginate(executionArn=arn, includeExecutionData=True):
            events.extend(page["events"])
        with gzip.open(folder / "history.json.gz", "wt") as stream:
            json.dump(events, stream, default=str)
        result = find_result(record.get("result_payload")) or find_result(json.loads(execution.get("output", "{}")))
        if not result:
            for event in reversed(events):
                details = event.get("stateExitedEventDetails", {})
                if details.get("name") == "FinalizeAnimation":
                    result = find_result(json.loads(details.get("output", "{}")))
                    if result:
                        break
        summary = {"job_id": job_id, "label": label, "status": execution["status"],
                   "execution_seconds": stamp(execution["stopDate"]) - stamp(execution["startDate"]),
                   "stages": state_times(events), "result": result,
                   "failed_task_events": sum(e["type"] in {"LambdaFunctionFailed", "TaskFailed"} for e in events)}
        for filename in ("input.json", "manifest.json"):
            try:
                body = self.s3.get_object(Bucket=self.outputs["WorkResultBucketName"], Key=f"jobs/{job_id}/prepare/{filename}")["Body"]
                try:
                    prepared = json.loads(body.read())
                finally:
                    body.close()
                write(folder / filename, prepared)
                if filename == "input.json":
                    summary["source_hashes"] = sorted({v["sha256"] for v in prepared.get("sources", []) if v.get("sha256")})
                    summary["character_renderer_sha256"] = prepared.get("character_renderer", {}).get("sha256")
                if filename == "manifest.json":
                    summary["prepared"] = {k: prepared.get(k) for k in ("viewbox", "frame_count", "frame_durations")}
            except ClientError as error:
                if error.response["Error"]["Code"] not in {"NoSuchKey", "404", "NotFound"}:
                    raise
        if self.collect_logs:
            self.profiles(job_id, execution, folder)
        write(summary_path, summary)
        return summary

    def profiles(self, job_id, execution, folder):
        prefix = f'/aws/lambda/aqw-char-{self.outputs.get("DeploymentEnvironment", "dev")}-'
        profiles = []
        for page in self.logs.get_paginator("describe_log_groups").paginate(logGroupNamePrefix=prefix):
            for group in page["logGroups"]:
                for result in self.logs.get_paginator("filter_log_events").paginate(
                    logGroupName=group["logGroupName"], filterPattern=f'"{job_id}"',
                    startTime=int((stamp(execution["startDate"]) - 60)*1000),
                    endTime=int((stamp(execution["stopDate"]) + 60)*1000)):
                    profiles.extend({"log_group":group["logGroupName"], **event} for event in result["events"])
        write(folder / "profiles.json", profiles)

    def configuration(self, path):
        # Explicitly capture CURRENT configuration, not alleged historical code.
        stage = self.outputs.get("DeploymentEnvironment", "dev")
        names = []
        for page in self.lambdas.get_paginator("list_functions").paginate():
            names.extend(f for f in page["Functions"] if f["FunctionName"].startswith(f"aqw-char-{stage}-"))
        fields = ("FunctionName", "CodeSha256", "RevisionId", "LastModified", "MemorySize", "Timeout", "Architectures")
        write(path, {"captured_at":utc_now(), "meaning":"configuration at capture time, not historical execution provenance",
                     "functions":[{k:f[k] for k in fields if k in f} for f in names]})


def comparison_warnings(group, summary):
    warnings = []
    if summary["result"].get("cache_hit"):
        warnings.append("Unexpected final-cache hit; exclude from render speed comparisons")
    reference = group["baselines"][0]["summary"]
    for key in ("width", "height", "frame_count", "duration_ms"):
        if summary["result"].get(key) != reference["result"].get(key):
            warnings.append(f"{key} differs from historical output")
    for key in ("source_hashes", "character_renderer_sha256"):
        if reference.get(key) is not None and summary.get(key) != reference[key]:
            warnings.append(f"{key} differs from historical input")
    for key in ("viewbox", "frame_durations"):
        if key in reference.get("prepared", {}) and summary.get("prepared", {}).get(key) != reference["prepared"][key]:
            warnings.append(f"{key} differs from historical prepare")
    return warnings


def variant_label(row):
    variant = row["variant"]
    if variant.startswith("avif-"):
        render = row["request"]["render"]
        return f'{variant} q{render["avif_quality"]} s{render["avif_speed"]}'
    return variant


def report(root, groups):
    rows = []
    for path in sorted((root / "runs").glob("*.json")):
        row = json.loads(path.read_text())
        if row.get("summary") and row["group_id"] in {g["id"] for g in groups}:
            rows.append(row)
    lines = ["# Render change comparison", "", "Historical samples are post-IIR; repeated identical inputs form one workload.",
             "Times are AWS execution wall time, not an isolated IIR measurement or summed parallel task time.", "",
             "| Workload | Variant | Successful samples | Median s | Range s | Median MiB | Historical time ratio |",
             "|---|---|---:|---:|---|---:|---:|"]
    for group in groups:
        baseline = [b["summary"] for b in group["baselines"]]
        successful = [b for b in baseline if b["status"]=="SUCCEEDED" and not b["result"].get("cache_hit")]
        ref = statistics.median([b["execution_seconds"] for b in successful]) if successful else None
        sets = [("post-IIR baseline", baseline)]
        variants = sorted({variant_label(r) for r in rows if r["group_id"] == group["id"]})
        sets += [(v,[r["summary"] for r in rows if r["group_id"]==group["id"] and variant_label(r)==v]) for v in variants]
        for variant, summaries in sets:
            ok = [s for s in summaries if s["status"]=="SUCCEEDED" and not s["result"].get("cache_hit")]
            times = [s["execution_seconds"] for s in ok]
            sizes = [s["result"]["bytes"]/1048576 for s in ok if "bytes" in s["result"]]
            median = statistics.median(times) if times else None
            ratio = f"{median/ref:.3f}x" if median is not None and ref and variant=="matched" and not any(comparison_warnings(group,s) for s in summaries) else "—"
            lines.append(f'| {group["name"]} ({group["id"][:6]}) | {variant} | {len(ok)}/{len(summaries)} | '
                         + (f'{median:.2f} | {min(times):.2f}–{max(times):.2f}' if times else '— | —')
                         + ' | ' + (f'{statistics.median(sizes):.3f}' if sizes else '—') + f' | {ratio} |')
    lines += ["", "A ratio below 1 means a faster matched workflow. Codec/export-cold variants intentionally change settings and receive no matched speedup claim.",
              "Cache state, deployment configuration, retries, and AWS noise can still affect timings. --no-render-cache is always applied to candidates.",
              "", "## Individual runs", ""]
    for row in rows:
        s = row["summary"]
        lines.append(f'- {row["name"]} / {row["variant"]} / round {row["round"]}: `{s["job_id"]}` {s["status"]}; [file]({s["result"].get("url", "")}).')
        if row.get("comparison_warnings"):
            lines.append("  WARNING: " + "; ".join(row["comparison_warnings"]))
    lines += ["", "See comparison.json and evidence/<job>/ for stage distributions, recorded requests, full histories, framing/timing, and optional Lambda profile logs.",
              "Lambda REPORT peak memory/billed GB-seconds and pixel-quality scores are not inferred from these wall times."]
    write(root / "comparison.json", {"groups":groups,"runs":rows})
    (root / "REPORT.md").write_text("\n".join(lines) + "\n")


def main():
    args = parser().parse_args()
    if args.rounds < 1 or args.timeout < 1 or args.poll <= 0 or not 0<=args.avif_quality<=100 or not 0<=args.avif_speed<=10:
        raise SystemExit("Invalid rounds, timeout, poll, quality or speed")
    boto3.setup_default_session(profile_name=args.profile, region_name=args.region)
    outputs = load_outputs(args.outputs.resolve())
    root = args.output_dir.resolve()
    root.mkdir(parents=True, exist_ok=True)
    evidence = Evidence(outputs, root, args.logs)
    frozen = root / "baselines.json"
    if frozen.exists():
        bundle = json.loads(frozen.read_text())
        sources = bundle["sources"]
        if args.checks_dir.is_dir():
            sources = discover(args.checks_dir, [REFERENCE, *(args.job or [])])
        elif args.job and not set(args.job).issubset(bundle["sources"]):
            raise SystemExit("Requested jobs are not in the frozen baseline bundle")
        if bundle["state_machine"] != outputs["StateMachineArn"] or set(bundle["sources"]) != set(sources):
            raise SystemExit("This output directory contains a different baseline set/account. Choose a new --output-dir.")
        baselines = bundle["baselines"]
    else:
        sources = discover(args.checks_dir, [REFERENCE, *(args.job or [])])
        (root / "operator-baselines").mkdir(exist_ok=True)
        for source in args.checks_dir.iterdir():
            if source.suffix in {".log", ".json"} and source.is_file():
                shutil.copy2(source, root / "operator-baselines" / source.name)
        baselines = []
        for job, label in sources.items():
            print(f"Capturing baseline {label}: {job}", flush=True)
            request, arn = load_original_request(outputs, job, evidence.jobs, evidence.sfn, evidence.s3)
            summary = evidence.collect(job, label)
            if summary["status"] != "SUCCEEDED" or summary["result"].get("cache_hit"):
                raise SystemExit(f"Baseline {job} is failed or a final-cache hit; it cannot be a render timing baseline")
            baselines.append({"request":request.to_dict(), "execution_arn":arn, "summary":summary})
        write(frozen, {"state_machine":outputs["StateMachineArn"],"sources":sources,"baselines":baselines})
        evidence.configuration(root / "configuration-at-baseline-capture.json")
    groups = group_baselines(baselines)
    if args.only:
        wanted = {name.strip() for name in args.only.casefold().split(",")}
        groups = [g for g in groups if wanted.intersection([g["name"], g["id"], *g["aliases"]])]
        missing = wanted - {name for g in groups for name in [g["name"], g["id"], *g["aliases"]]}
        if missing:
            raise SystemExit(f"Unknown workloads: {sorted(missing)}")
    variants = SUITES[args.suite] + (["avif-lossless"] if args.include_lossless else [])
    print(f"{len(baselines)} historical samples → {len(groups)} selected workloads; {len(groups)*len(variants)*args.rounds} candidate jobs.")
    for g in groups:
        print(f'  {g["name"]}: {len(g["baselines"])} historical samples, cache={g["request"]["cache"]}')
    write(root / "plan.json", {"groups":[g["id"] for g in groups],"variants":variants,"rounds":args.rounds})
    report(root, groups)
    if not args.run:
        print(f"Captured only; no jobs submitted. Review {root / 'REPORT.md'}; add --run after deployment.")
        return 0
    tag = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    evidence.configuration(root / f"configuration-before-{tag}.json")
    failures = 0
    for round_ in range(1, args.rounds+1):
        # Rotate modes across rounds to reduce systematic warm-order bias.
        order = variants[(round_-1)%len(variants):] + variants[:(round_-1)%len(variants)]
        for group in groups:
            for variant in order:
                # Encoding settings are part of the run identity, so changing
                # q/s does not silently resume a different experiment.
                key = f'{group["id"]}-{variant}-q{args.avif_quality}-s{args.avif_speed}-r{round_}'
                path = root / "runs" / f"{key}.json"
                if path.exists():
                    row = json.loads(path.read_text())
                    request = JobRequest.from_dict(row["request"])
                else:
                    request = candidate(JobRequest.from_dict(group["request"]),variant,args.avif_quality,args.avif_speed)
                    row = {"name":group["name"],"group_id":group["id"],"variant":variant,"round":round_,"request":request.to_dict(),"phase":"planned"}
                    write(path,row)  # journal the ID before admission
                if row.get("summary"):
                    failures += row["summary"]["status"] != "SUCCEEDED"
                    continue
                record = evidence.jobs.get(request.job_id)
                if record is None:
                    if row["phase"] != "planned":
                        raise RuntimeError("Previously queued job is missing; refusing an ambiguous resubmission")
                    enqueue_request(outputs,request,2)
                    row["phase"]="queued"
                    write(path,row)
                print(f'{group["name"]} / {variant} / round {round_}: {request.job_id}',flush=True)
                if record is None or record.get("status") not in TERMINAL_STATUSES:
                    record = wait_for(outputs,request.job_id,timeout_seconds=args.timeout,poll_seconds=args.poll)
                # Never launch the next job after a timeout; rerun this command
                # to resume the same journaled ID rather than creating repeats.
                row["summary"] = evidence.collect(request.job_id,group["name"],wait_running=True)
                row["comparison_warnings"] = comparison_warnings(group, row["summary"])
                row["phase"]="complete"
                write(path,row)
                failures += row["summary"]["status"] != "SUCCEEDED"
                report(root,groups)
    evidence.configuration(root / f"configuration-after-{tag}.json")
    print(f"Finished: {root / 'REPORT.md'}",flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
