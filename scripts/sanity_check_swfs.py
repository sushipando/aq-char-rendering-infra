#!/usr/bin/env python3
"""Read-only local corpus check using the production Rust parsers; no AWS."""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
SKIP = {'.git', '.venv', 'node_modules', 'target', '__pycache__'}


def discover(roots):
    seen = set()
    for root in roots:
        if root.is_file():
            candidates = [root]
        else:
            candidates = []
            for folder, dirs, files in os.walk(root):
                dirs[:] = sorted(d for d in dirs if d not in SKIP)
                candidates.extend(Path(folder)/f for f in sorted(files) if f.lower().endswith('.swf'))
        for path in candidates:
            path = path.resolve()
            if path not in seen and path.suffix.lower() == '.swf':
                seen.add(path)
                yield path


def run_one(binary, path, jar, timeout):
    command = [str(binary), str(path)] + ([str(jar)] if jar else [])
    started = time.monotonic()
    # Capture to disk: a malformed input must not flood parent memory. A process
    # group lets the outer deadline terminate Rust AND its FFDec child.
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(command, stdout=output, stderr=errors, start_new_session=True)
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            return {'status':'timeout','seconds':round(time.monotonic()-started,3)}
        output.seek(0)
        try:
            result = json.loads(output.read(8 * 1024 * 1024))
            if process.returncode:
                result = {'status':'process_failed','exit_code':process.returncode}
        except (ValueError, UnicodeError):
            errors.seek(0)
            result = {'status':'process_failed','exit_code':process.returncode,
                      'error':errors.read(4096).decode(errors='replace')}
    result['seconds'] = round(time.monotonic()-started,3)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('roots', nargs='*', type=Path, default=[Path.home()/'Projects/aq'])
    parser.add_argument('--ffdec', type=Path, help='FFDec JAR; required unless --structural-only')
    parser.add_argument('--structural-only', action='store_true', help='Skip decompilation and ActionScript/timeline resolution')
    parser.add_argument('--workers', type=int, default=2, help='Concurrent SWFs/Java processes (default 2)')
    parser.add_argument('--limit', type=int, help='Only first N discovered files (before content deduplication)')
    parser.add_argument('--timeout', type=int, default=120, help='Per-SWF wall timeout in seconds; FFDec internal limit is 90')
    parser.add_argument('--output', type=Path, help='New results directory; refuses existing paths')
    parser.add_argument('--no-build', action='store_true', help='Use an already built native helper')
    args = parser.parse_args()
    if not 1 <= args.workers <= 8 or args.timeout < 1 or (args.limit is not None and args.limit < 1):
        parser.error('workers must be 1–8; timeout and limit must be positive')
    roots = [p.expanduser().resolve() for p in args.roots]
    if not all(p.exists() for p in roots):
        parser.error('Every input root must exist')
    jar = None if args.structural_only else args.ffdec
    if not args.structural_only and (jar is None or not jar.expanduser().is_file()):
        parser.error('Supply --ffdec /path/to/ffdec.jar or choose --structural-only')
    jar = jar.expanduser().resolve() if jar else None
    manifest = REPO/'services/pipeline-rust/Cargo.toml'
    binary = REPO/'services/pipeline-rust/target/debug/examples/swf_sanity'
    if not args.no_build:
        # Build the host target, never the ARM Lambda images. Override an inherited
        # cargo target/target directory so the executable path is unambiguous.
        env = {k:v for k,v in os.environ.items() if k not in {'CARGO_BUILD_TARGET','CARGO_TARGET_DIR'}}
        subprocess.run(['cargo','build','--manifest-path',str(manifest),'--target-dir',str(binary.parents[2]),
                        '--example','swf_sanity'], check=True, env=env)
    if not binary.is_file():
        parser.error('Native helper missing; run without --no-build')
    out = args.output.expanduser().resolve() if args.output else Path(tempfile.mkdtemp(prefix='aqw-swf-sanity-'))
    if args.output:
        out.mkdir(parents=True, exist_ok=False)
    print(f'Results: {out}', flush=True)
    groups, unreadable = {}, []
    for index, path in enumerate(discover(roots)):
        if args.limit is not None and index >= args.limit:
            break
        try:
            with path.open('rb') as source:
                digest = hashlib.file_digest(source,'sha256').hexdigest()
            # Backgrounds use the production wrapper and policy, unlike items.
            group = (digest, path.name.startswith('cp-bg'))
            groups.setdefault(group, []).append(str(path))
        except OSError as error:
            unreadable.append({'status':'unreadable','paths':[str(path)],'error':str(error)})
    if not groups and not unreadable:
        raise SystemExit('No SWFs found in the supplied roots')
    print(f'{sum(map(len,groups.values())) + len(unreadable)} files; {len(groups)} unique contents/policies', flush=True)
    counts = Counter()
    with (out/'results.jsonl').open('w') as report:
        def save(result):
            counts[result['status']] += 1
            report.write(json.dumps(result)+'\n')
            report.flush()
        for result in unreadable:
            save(result)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = {pool.submit(run_one,binary,Path(paths[0]),jar,args.timeout):(digest,paths)
                       for (digest,_),paths in groups.items()}
            for n, future in enumerate(as_completed(futures),1):
                digest, paths = futures[future]
                try:
                    result = future.result()
                except Exception as error:
                    result = {'status':'checker_failed','error':str(error)}
                save({**result,'sha256':digest,'paths':paths})
                if n % 25 == 0 or n == len(futures):
                    print(f'{n}/{len(futures)} unique files: {dict(counts)}',flush=True)
    summary = {'created_at':datetime.now().astimezone().isoformat(), 'roots':list(map(str,roots)),
               'mode':'structural' if args.structural_only else 'scripts-and-timelines',
               'ffdec':str(jar) if jar else None, 'counts':dict(counts),
               'files':sum(map(len,groups.values()))+len(unreadable),'unique_checks':len(groups)}
    (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
    (out/'REPORT.md').write_text('# Local SWF sanity check\n\n'+f"Mode: {summary['mode']}\n\n"+
        '\n'.join(f'- {status}: {count}' for status,count in sorted(counts.items()))+
        '\n\nDetails and duplicate paths: `results.jsonl`. Counts above are per unique content/policy.\n\n'
        '`timeline_review` tests each exported sprite independently, not a real job’s full root selection. '
        'It can include unused UI/walking states. `no_exported_roots` is not a timeline pass. '
        'Structural-only results do not validate ActionScript. No SVG export, raster, pixel, or AWS performance validation is performed.\n')
    print(f'Done: {out / "REPORT.md"}', flush=True)
    return 1 if any(status != 'ok' for status in counts) else 0


if __name__ == '__main__':
    raise SystemExit(main())
