#!/usr/bin/env python3
"""Create a standalone fetch Lambda payload; no network or render submission."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
from uuid import UUID, uuid4

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('username')
parser.add_argument('--view', choices=['character', 'charpage'], default='charpage')
parser.add_argument('--job-id', type=UUID)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
job = str(args.job_id or uuid4())
payload = {'schema_version': 1, 'job_id': job,
           'created_at': datetime.now(timezone.utc).isoformat(),
           'discord': {'user_id': '1', 'channel_id': '1'},
           'render': {'username': args.username, 'view': args.view}}
args.output.write_text(json.dumps(payload, indent=2) + '\n')
print(json.dumps({'job_id': job, 'payload': str(args.output),
                  'request_key': f'jobs/{job}/fetch/request.json',
                  'sources_key': f'jobs/{job}/fetch/sources.json'}))
