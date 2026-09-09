import importlib.util
from pathlib import Path
import sys

SPEC = importlib.util.spec_from_file_location('sanity', Path(__file__).parents[1]/'sanity_check_swfs.py')
sanity = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sanity)


def test_discovery_includes_ignored_caches_and_deduplicates_paths(tmp_path):
    cache = tmp_path/'.cache'; cache.mkdir()
    swf = cache/'example.SWF'; swf.write_bytes(b'fixture')
    skipped = tmp_path/'target'; skipped.mkdir()
    (skipped/'not-a-source.swf').write_bytes(b'fixture')
    assert list(sanity.discover([tmp_path,swf])) == [swf]


def test_helper_protocol_and_bad_output(tmp_path):
    helper = tmp_path/'helper.py'
    helper.write_text('print(\'{"status":"ok","sprites":3}\')')
    result = sanity.run_one(Path(sys.executable),helper,None,5)
    assert result['status'] == 'ok' and result['sprites'] == 3
    helper.write_text('print("not json")')
    assert sanity.run_one(Path(sys.executable),helper,None,5)['status'] == 'process_failed'


def test_helper_timeout_is_reported(tmp_path):
    helper = tmp_path/'helper.py'
    helper.write_text('import time; time.sleep(60)')
    assert sanity.run_one(Path(sys.executable),helper,None,0.05)['status'] == 'timeout'
