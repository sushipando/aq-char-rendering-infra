import hashlib
import json
from pathlib import Path
from unittest.mock import Mock, patch
from uuid import uuid4

import pytest
from aqw_char_renderer import fetch_sources as fetch
from aqw_char_renderer.contracts import JobRequest, RenderSettings, DiscordTarget, ItemOverride
from aqw_char_renderer.storage import FilesystemObjectStore


class Store(FilesystemObjectStore):
    def __init__(self, root):
        super().__init__(root)
        self.metadata = {}
    def exists(self, bucket, key):
        result = super().exists(bucket, key)
        return {**result, 'Metadata': self.metadata.get((bucket,key),{})} if result else None
    def upload_file_if_absent(self, source, bucket, key, **kwargs):
        created = super().upload_file_if_absent(source, bucket, key)
        if created: self.metadata[bucket,key] = kwargs.get('metadata',{})
        return created


@pytest.fixture
def setup(tmp_path):
    store = Store(tmp_path)
    data = [{'id':7,'slot':'helm','file':'items/helms/override.swf','name':'Override'}]
    raw = json.dumps(data).encode()
    path = store._path('source','items.json'); path.parent.mkdir(parents=True); path.write_bytes(raw)
    record = {'key':'items.json','size':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}
    store.write_json('source','datasets/test/manifest.json', {'schema_version':1,'dataset_version':'test',
        'assets':{},'item_database':record,'character_renderer':record})
    request = JobRequest(job_id=str(uuid4()),created_at='2026-09-07T00:00:00Z',
                         discord=DiscordTarget(user_id='1',channel_id='2'),render=RenderSettings(username='Alina'))
    return store, request.to_dict()


def execute(store, payload, fields):
    def download(url, destination, *, timeout):
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(b'FWS12345')
    getter = Mock(return_value=fields)
    with patch.object(fetch.tryon,'download_swf',side_effect=download) as downloader:
        result = fetch.fetch_request(payload, store=store, source_bucket='source',work_bucket='work',
                                     dataset='test',proxy_config=None,appearance_fetcher=getter)
    return result, getter, downloader


def test_fetch_populates_request_caches_sources_and_reuses_completed_step(setup):
    store, payload = setup
    fields = {'strName':'Alina','strGender':'F','strClassFile':'armor.swf','strPetFile':'pets/bank.swf'}
    result, getter, downloader = execute(store,payload,fields)
    assert payload['appearance'] is None
    assert result['appearance'] == fields
    assert getter.call_count == 1 and downloader.call_count == 2
    saved = store.read_json('work',f"jobs/{payload['job_id']}/fetch/sources.json")
    assert {v['path'] for v in saved['sources']} == {'classes/F/armor.swf','pets/bank.swf'}
    _, getter, downloader = execute(store,payload,fields)
    assert getter.call_count == downloader.call_count == 0
    # Different job fetches fresh character data but reuses immutable source objects.
    payload['job_id'] = str(uuid4())
    _, getter, downloader = execute(store,payload,fields)
    assert getter.call_count == 1 and downloader.call_count == 0


def test_override_visibility_base_items_and_background_planning(setup, tmp_path, monkeypatch):
    store, payload = setup
    fields = {'strName':'Alina','strGender':'M','strClassFile':'base.swf','strCustArmorName':'Cosmetic',
              'strCustArmorFile':'cosmetic.swf','strHelmFile':'old.swf','strPetFile':'pet.swf','ia1':'7','bgindex':'W'}
    payload['render'].update(base_items=True,show_hidden=True,view='charpage',override={'item_id':7,'slot':'helm'})
    catalog = tmp_path/'background.json'
    catalog.write_text(json.dumps([{'index':32,'file':'cp-bg32.swf','sha256':hashlib.sha256(b'FWS12345').hexdigest()}]))
    monkeypatch.setenv('AQW_BACKGROUND_CATALOG',str(catalog))
    execute(store,payload,fields)
    records = store.read_json('work',f"jobs/{payload['job_id']}/fetch/sources.json")['sources']
    assert {v['path'] for v in records} == {'classes/M/base.swf','items/helms/override.swf','pet.swf','etc/chardetail/bgs/cp-bg32.swf'}


def test_retry_loads_snapshot_inside_aws(setup):
    store, payload = setup
    fields = {'strName':'Alina','strClassFile':'armor.swf'}
    execute(store,payload,fields)
    payload['source_job_id'] = payload['job_id']; payload['job_id'] = str(uuid4())
    payload['appearance_overrides'] = {'intColorHair':'0'}
    result, getter, downloader = execute(store,payload,None)
    assert getter.call_count == downloader.call_count == 0
    assert result['appearance']['intColorHair'] == '0'


def test_failure_does_not_publish_successful_fetch(setup):
    store, payload = setup
    with patch.object(fetch.tryon,'download_swf',side_effect=RuntimeError('fixture failed')):
        with pytest.raises(RuntimeError):
            fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',dataset='test',
                                proxy_config=None,appearance_fetcher=lambda *a,**kw: {'strName':'Alina','strClassFile':'armor.swf'})
    assert store.exists('work',f"jobs/{payload['job_id']}/fetch/request.json") is None


def test_packaged_background_catalog_matches_rust():
    root = Path(__file__).resolve().parents[2]
    assert json.loads((root/'pipeline-rust/assets/charpage/sources.json').read_text()) == json.loads(
        Path(fetch.__file__).with_name('background_sources.json').read_text())


def test_hidden_character_and_bad_proxy_fail_without_a_completed_request(setup):
    store, payload = setup
    getter = Mock(side_effect=RuntimeError('character unavailable'))
    with pytest.raises(RuntimeError, match='unavailable'):
        fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',dataset='test',
                            proxy_config=None,appearance_fetcher=getter)
    assert store.exists('work',f"jobs/{payload['job_id']}/fetch/request.json") is None


def test_failed_asset_attempt_keeps_snapshot_for_retry(setup):
    store, payload = setup
    fields = {'strName':'Alina','strClassFile':'armor.swf'}
    with patch.object(fetch.tryon,'download_swf',side_effect=RuntimeError('fixture')):
        with pytest.raises(RuntimeError):
            fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',dataset='test',
                                proxy_config=None,appearance_fetcher=lambda *a,**kw: fields)
    original = payload['job_id']
    assert store.exists('work',f'jobs/{original}/fetch/appearance.json')
    payload.update(job_id=str(uuid4()),source_job_id=original)
    result, getter, _ = execute(store,payload,None)
    assert result['appearance'] == fields
    getter.assert_not_called()


def test_pinned_background_mismatch_is_not_uploaded(setup, monkeypatch, tmp_path):
    store,payload=setup
    payload['render'].update(view='charpage')
    records=tmp_path/'bad-bg.json'
    records.write_text(json.dumps([{'index':32,'file':'bg.swf','sha256':'0'*64}]))
    monkeypatch.setenv('AQW_BACKGROUND_CATALOG',str(records))
    with pytest.raises(fetch.SourceAssetError,match='checksum'):
        execute(store,payload,{'strName':'Alina','strClassFile':'armor.swf','bgindex':'W'})
    path='etc/chardetail/bgs/bg.swf'
    digest=hashlib.sha256(path.encode()).hexdigest()
    assert store.exists('source',f'dynamic-assets/test/{digest[:2]}/{digest}.swf') is None


def test_transient_asset_failure_restarts_session_after_all_uploads_finish(setup, monkeypatch):
    from collections import Counter
    from threading import Barrier
    from aqw_char_renderer import source_http as http
    store, payload = setup
    fields = {'strName':'Alina','strClassFile':'armor.swf','strPetFile':'pets/bank.swf'}
    calls, sessions, page_sessions = Counter(), {}, []
    barrier = Barrier(2)
    def page(*args, **kwargs):
        page_sessions.append(http.session_username('user'))
        return fields
    def download(url, destination, *, timeout):
        calls[url] += 1
        sessions.setdefault(url, []).append(http.session_username('user'))
        if calls[url] == 1:
            barrier.wait(timeout=3)  # Would fail if downloads were serialized.
        if 'pets/bank.swf' in url and calls[url] == 1:
            raise http.SourceConnectionError('transient fixture')
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(b'FWS12345')
    monkeypatch.setattr(fetch.time, 'sleep', lambda _: None)
    with patch.object(fetch.tryon, 'download_swf', side_effect=download):
        result = fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',
                                    dataset='test',proxy_config=None,appearance_fetcher=page)
    assert result['appearance'] == fields
    assert len(page_sessions) == 2 and page_sessions[0] != page_sessions[1]
    assert sorted(calls.values()) == [1,2]
    assert all(names[0] == page_sessions[0] for names in sessions.values())
    assert next(names for names in sessions.values() if len(names) == 2)[1] == page_sessions[1]
    records = store.read_json('work',f"jobs/{payload['job_id']}/fetch/sources.json")['sources']
    assert sorted(r['cache_hit'] for r in records) == [False,True]


def test_charpage_transport_failure_is_retried_and_bounded(setup, monkeypatch):
    from aqw_char_renderer import source_http as http
    store,payload = setup
    monkeypatch.setattr(fetch.time,'sleep',lambda _: None)
    getter = Mock(side_effect=http.SourceConnectionError('transient'))
    with pytest.raises(http.SourceConnectionError):
        fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',dataset='test',
                            proxy_config=None,appearance_fetcher=getter)
    assert getter.call_count == 3
    assert store.exists('work',f"jobs/{payload['job_id']}/fetch/request.json") is None


def test_snapshot_recovery_bootstraps_new_page_without_changing_outfit(setup, monkeypatch):
    from aqw_char_renderer import source_http as http
    store,payload = setup
    snapshot = {'strName':'Alina','strClassFile':'original.swf'}
    payload['appearance'] = snapshot
    getter = Mock(return_value={'strName':'Alina','strClassFile':'changed.swf'})
    calls = []
    def download(url, destination, *, timeout):
        calls.append(url)
        if len(calls) == 1:
            raise http.SourceConnectionError('transient')
        destination.parent.mkdir(parents=True,exist_ok=True)
        destination.write_bytes(b'FWS12345')
    monkeypatch.setattr(fetch.time,'sleep',lambda _: None)
    with patch.object(fetch.tryon,'download_swf',side_effect=download):
        result = fetch.fetch_request(payload,store=store,source_bucket='source',work_bucket='work',dataset='test',
                                    proxy_config=None,appearance_fetcher=getter)
    assert result['appearance'] == snapshot
    assert getter.call_count == 1
    assert all('original.swf' in url for url in calls)


@pytest.mark.parametrize('status,retry',[(403,True),(408,True),(429,True),(502,True),(404,False),(401,False)])
def test_http_retry_classification(status,retry):
    from urllib.error import HTTPError
    assert fetch._retryable(HTTPError('https://game.aq.com/x',status,'fixture',{},None)) == retry
