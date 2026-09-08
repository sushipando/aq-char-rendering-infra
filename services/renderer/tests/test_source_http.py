import json
from unittest.mock import Mock, patch
from urllib.error import HTTPError

import pytest
from aqw_char_renderer import source_http as http

JOB = '45cfafbd-5089-4f6d-850a-caa798ec1fcb'
URL = 'https://game.aq.com/game/gamefiles/test.swf'


def fetch(limit=16):
    return http.fetch_bytes(URL, timeout=5, maximum_bytes=limit, user_agent='old-bot-UA')


def test_one_cookie_session_and_browser_profile_for_whole_job():
    with patch.object(http.requests, 'Session') as factory:
        response = Mock(status_code=200, headers={})
        def get(*args, **kwargs):
            kwargs['content_callback'](b'FWS\x00\xff\x80\x00\x00')
            assert kwargs['allow_redirects'] is False
            assert 'User-Agent' not in kwargs['headers']
            return response
        factory.return_value.get.side_effect = get
        with http.source_session(JOB, proxy_config=json.dumps({'username':'user','password':'password'})):
            assert fetch() == fetch() == b'FWS\x00\xff\x80\x00\x00'
        factory.assert_called_once()
        assert factory.call_args.kwargs['proxy_auth'] == ('user-session-45cfafbd50894f6d850acaa798ec1fcb-const','password')
        assert factory.call_args.kwargs['impersonate'] == 'chrome136'
        assert factory.call_args.kwargs['trust_env'] is False
        factory.return_value.close.assert_called_once()
        assert http.session_username('user') == 'user'


@pytest.mark.parametrize('config', ['', '[]', '{"username":"secret"}',
    '{"username":"u-session-old","password":"secret"}',
    '{"username":"u","password":"secret","server":"http://secret@host:33335"}',
    '{"username":"u","password":"secret","ca_pem":"secret"}'])
def test_config_fails_without_disclosing_secrets(config):
    with http.source_session(JOB, proxy_config=config), pytest.raises(http.SourceHttpError) as error:
        fetch()
    assert 'secret' not in str(error.value)


def test_status_redirect_bounds_and_error_sanitization():
    with patch.object(http.requests, 'Session') as factory, http.source_session(JOB):
        factory.return_value.get.return_value = Mock(status_code=403, headers={})
        with pytest.raises(HTTPError) as error:
            fetch()
        assert error.value.code == 403
        factory.return_value.get.return_value = Mock(status_code=302, headers={'Location':'http://game.aq.com/x'})
        with pytest.raises(http.SourceHttpError, match='redirected'):
            fetch()
        def overflow(*args, **kwargs):
            assert kwargs['content_callback'](b'x' * 17) == 0
            raise http.requests.RequestsError('secret')
        factory.return_value.get.side_effect = overflow
        with pytest.raises(http.SourceHttpError, match='exceeded'):
            fetch()
        factory.return_value.get.side_effect = http.requests.RequestsError('secret')
        with pytest.raises(http.SourceHttpError) as error:
            fetch()
        assert 'secret' not in str(error.value)


def test_concurrent_contexts_and_threads_are_isolated():
    import asyncio
    async def run(job):
        with http.source_session(job):
            await asyncio.sleep(0)
            return await asyncio.to_thread(http.session_username, 'user')
    async def check():
        names = await asyncio.gather(run(JOB), run('45cfafbd-5089-4f6d-850a-caa798ec1fca'))
        assert names[0] != names[1]
        assert http.session_username('user') == 'user'
    asyncio.run(check())


def test_real_curl_connect_auth_no_proxy_and_cookies(tmp_path, monkeypatch):
    import base64
    import socketserver
    import ssl
    import subprocess
    import threading
    cert, key = tmp_path / 'cert.pem', tmp_path / 'key.pem'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1',
                    '-keyout',str(key),'-out',str(cert),'-subj','/CN=game.aq.com',
                    '-addext','subjectAltName=DNS:game.aq.com'], check=True, capture_output=True)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    tls.set_alpn_protocols(['http/1.1'])
    tunnels, requests = [], []
    parallel = threading.Barrier(2)
    def headers(reader):
        lines = []
        while line := reader.readline():
            if line == b'\r\n': break
            lines.append(line)
        return b''.join(lines)
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.settimeout(5)
            tunnels.append(headers(self.request.makefile('rb')))
            self.request.sendall(b'HTTP/1.1 200 Connection Established\r\n\r\n')
            with tls.wrap_socket(self.request, server_side=True) as conn:
                request = headers(conn.makefile('rb'))
                requests.append(request)
                if b'parallel=1' in request:
                    parallel.wait(timeout=3)
                conn.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 8\r\nSet-Cookie: aqw=test; Path=/; Secure\r\nConnection: close\r\n\r\nFWS12345')
    with socketserver.ThreadingTCPServer(('127.0.0.1',0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        monkeypatch.setenv('NO_PROXY','*')
        config = json.dumps({'server':f'http://127.0.0.1:{server.server_address[1]}',
                             'username':'user','password':'pass','ca_pem':cert.read_text()})
        try:
            with http.source_session(JOB, proxy_config=config):
                assert fetch() == fetch() == b'FWS12345'
                from concurrent.futures import ThreadPoolExecutor
                from contextvars import copy_context
                def worker():
                    with http.source_worker_session():
                        return http.fetch_bytes(URL + '?parallel=1', timeout=5, maximum_bytes=16, user_agent='')
                with ThreadPoolExecutor(max_workers=2) as pool:
                    futures = [pool.submit(copy_context().run, worker) for _ in range(2)]
                    assert [f.result() for f in futures] == [b'FWS12345',b'FWS12345']
        finally:
            server.shutdown()
            thread.join()
    assert len(tunnels) == len(requests) == 4
    auth = base64.b64encode(b'user-session-45cfafbd50894f6d850acaa798ec1fcb-const:pass')
    assert all(auth in tunnel for tunnel in tunnels)
    assert all(b'cookie: aqw=test' in request.lower() for request in requests[1:])
    assert b'Chrome/136' in requests[0]
    assert all(b'proxy-authorization' not in req.lower() for req in requests)
