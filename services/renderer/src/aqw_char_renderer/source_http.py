"""AQW HTTPS fetching with one curl_cffi browser/proxy/cookie session per job."""
from __future__ import annotations

from contextlib import contextmanager
from contextvars import ContextVar
import json
import os
from pathlib import Path
import tempfile
import threading
import time
from urllib.error import HTTPError
from urllib.parse import urljoin, urlsplit
from uuid import UUID, uuid4

import certifi
from curl_cffi import CurlOpt, requests


class SourceHttpError(RuntimeError):
    pass


class SourceConnectionError(SourceHttpError):
    """Transient transport failure eligible for a fresh acquisition session."""
    pass


def _official(url: str) -> bool:
    try:
        parsed = urlsplit(url)
        return (parsed.scheme == "https" and parsed.hostname in {"account.aq.com", "game.aq.com"}
                and parsed.port in {None, 443} and parsed.username is None and parsed.password is None)
    except ValueError:
        return False


class _SourceSession:
    def __init__(self, job_id, config):
        self.identifier = UUID(job_id).hex
        self.config = config
        self.client = None
        self.ca_directory = None
        self.lock = threading.RLock()
        self.deadline = None
        self.cookies = None

    def username(self, base):
        if "-session-" in base.lower() or "const" in base.lower().split("-"):
            raise SourceHttpError("Configure a base Bright Data username without session or const options")
        return f"{base}-session-{self.identifier}-const"

    def open(self):
        if self.client is not None:
            return self.client
        options = {"impersonate": "chrome136", "trust_env": False,
                   "curl_options": {CurlOpt.PROXY: "", CurlOpt.NOPROXY: ""}}
        if self.config is not None:
            try:
                config = json.loads(self.config)
                if not isinstance(config, dict) or set(config) - {"server", "username", "password", "ca_pem"}:
                    raise ValueError()
                user, password = config["username"], config["password"]
                if not isinstance(user, str) or not user or ":" in user or not isinstance(password, str) or not password:
                    raise ValueError()
                server = config.get("server", "http://brd.superproxy.io:33335")
                parsed = urlsplit(server)
                if (parsed.scheme != "http" or not parsed.hostname or parsed.port is None
                        or parsed.username is not None or parsed.password is not None
                        or parsed.path not in {"", "/"} or parsed.query or parsed.fragment):
                    raise ValueError()
                options["proxy"] = server
                options["proxy_auth"] = (self.username(user), password)
                options["curl_options"] = {CurlOpt.NOPROXY: ""}
                if config.get("ca_pem"):
                    import ssl
                    ssl.create_default_context().load_verify_locations(cadata=config["ca_pem"])
                    self.ca_directory = tempfile.TemporaryDirectory(prefix="aqw-ca-")
                    ca = Path(self.ca_directory.name) / "ca.pem"
                    ca.write_text(Path(certifi.where()).read_text() + "\n" + config["ca_pem"])
                    options["verify"] = str(ca)
            except (ValueError, TypeError, KeyError, OSError):
                raise SourceHttpError("Invalid AQW_BRIGHTDATA_CONFIG (credentials/server/CA)") from None
        if self.cookies is not None:
            options["cookies"] = self.cookies
        self.client = requests.Session(**options)
        return self.client

    def close(self):
        if self.client is not None:
            self.client.close()
        if self.ca_directory is not None:
            self.ca_directory.cleanup()


_session = ContextVar("aqw_source_session", default=None)


@contextmanager
def source_session(job_id: str, *, proxy_config: str | None = None, deadline=None):
    session = _SourceSession(job_id, proxy_config if proxy_config is not None
                             else os.environ.get("AQW_BRIGHTDATA_CONFIG"))
    session.deadline = deadline
    token = _session.set(session)
    try:
        yield
    finally:
        _session.reset(token)
        session.close()


@contextmanager
def source_worker_session():
    """Independent curl handle/cookie copy, same sticky IP as the charpage.

    Called in a copied ContextVar context. No network request holds the parent's
    lock, and every worker closes its own client on its owning thread.
    """
    parent = _session.get()
    if parent is None:
        raise SourceHttpError("Asset worker requires an acquisition session")
    child = _SourceSession(parent.identifier, parent.config)
    child.deadline = parent.deadline
    token = _session.set(child)
    try:
        with parent.lock:
            # Cache hits create neither a curl client nor a proxy connection.
            if parent.client is not None:
                child.cookies = requests.Cookies(parent.client.cookies)
        yield
    finally:
        _session.reset(token)
        child.close()


def session_username(username: str) -> str:
    session = _session.get()
    return session.username(username) if session else username


def fetch_bytes(url: str, *, timeout: float, maximum_bytes: int, user_agent: str,
                accept: str = "*/*") -> bytes:
    # Keep the caller signature, but let the pinned browser profile supply its UA.
    del user_agent
    if not _official(url):
        raise SourceHttpError("AQW fetch requires an allowed HTTPS origin")
    if _session.get() is None:
        with source_session(str(uuid4())):
            return fetch_bytes(url, timeout=timeout, maximum_bytes=maximum_bytes,
                               user_agent="", accept=accept)
    session = _session.get()
    with session.lock:
        client = session.open()
        deadline = min(time.monotonic() + timeout, session.deadline or float("inf"))
        for _ in range(11):
            data = bytearray()
            oversized = False

            def receive(chunk):
                nonlocal oversized
                if len(data) + len(chunk) > maximum_bytes:
                    oversized = True
                    return 0
                data.extend(chunk)
                return len(chunk)

            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SourceConnectionError("AQW source request timed out")
            try:
                response = client.get(url, timeout=remaining, allow_redirects=False,
                                      headers={"Accept": accept}, content_callback=receive)
            except requests.RequestsError as error:
                if oversized:
                    raise SourceHttpError("AQW response exceeded the allowed size") from None
                raise SourceConnectionError(f"AQW source connection failed (curl code {int(error.code)}); check proxy, TLS and connectivity") from None
            try:
                if oversized:
                    raise SourceHttpError("AQW response exceeded the allowed size")
                status = response.status_code
                if status in {301, 302, 303, 307, 308}:
                    target = urljoin(url, response.headers.get("Location", ""))
                    if not response.headers.get("Location") or not _official(target):
                        raise SourceHttpError("AQW redirected outside the allowed HTTPS origins")
                    url = target
                    continue
                if not 200 <= status < 300:
                    raise HTTPError(url, status, "AQW source HTTP request failed", {}, None)
                return bytes(data)
            finally:
                response.close()
        raise SourceHttpError("AQW source exceeded the redirect limit")
