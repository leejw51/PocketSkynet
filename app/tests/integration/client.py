"""Thin HTTP client for the integration suite — stdlib urllib only.

Proxies are disabled explicitly: a developer's HTTP(S)_PROXY must never sit
between the suite and the loopback server it just started (the Rust harness
does the same).
"""

import json
import ssl
import urllib.error
import urllib.parse
import urllib.request

_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _opener_for(ca_file: str | None):
    if ca_file is None:
        return _OPENER
    # Trust exactly the CA the server minted for itself — not the system
    # store, and with hostname/IP verification left on, because the whole
    # point is proving a real client could trust this deployment.
    context = ssl.create_default_context(cafile=ca_file)
    return urllib.request.build_opener(
        urllib.request.ProxyHandler({}),
        urllib.request.HTTPSHandler(context=context),
    )


class Resp:
    def __init__(self, status, headers, body: bytes):
        self.status = status
        self.headers = headers
        self.body = body

    def json(self):
        return json.loads(self.body.decode("utf-8"))

    def expect(self, status, why=""):
        if self.status != status:
            raise AssertionError(
                f"expected HTTP {status}, got {self.status}{' — ' + why if why else ''}: "
                f"{self.body[:500]!r}"
            )
        return self


class Api:
    """A base URL plus an optional bearer token."""

    def __init__(
        self, base_url: str, token: str | None = None, ca_file: str | None = None
    ):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.ca_file = ca_file
        self._opener = _opener_for(ca_file)

    def request(
        self,
        method,
        path,
        json_body=None,
        raw_body=None,
        headers=None,
        token="inherit",
        timeout=30,
    ) -> Resp:
        url = self.base_url + path
        hdrs = dict(headers or {})
        data = None
        if json_body is not None:
            data = json.dumps(json_body).encode("utf-8")
            hdrs.setdefault("Content-Type", "application/json")
        elif raw_body is not None:
            data = raw_body
        tok = self.token if token == "inherit" else token
        if tok:
            hdrs.setdefault("Authorization", f"Bearer {tok}")
        req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
        try:
            with self._opener.open(req, timeout=timeout) as resp:
                return Resp(resp.status, resp.headers, resp.read())
        except urllib.error.HTTPError as err:
            return Resp(err.code, err.headers, err.read())

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, json_body=None, **kw):
        return self.request("POST", path, json_body=json_body, **kw)

    def put(self, path, json_body=None, **kw):
        return self.request("PUT", path, json_body=json_body, **kw)

    def patch(self, path, json_body=None, **kw):
        return self.request("PATCH", path, json_body=json_body, **kw)

    def delete(self, path, **kw):
        return self.request("DELETE", path, **kw)

    def open_stream(self, path, headers=None, timeout=20):
        """Open a long-lived response (SSE) and return the raw HTTPResponse."""
        hdrs = dict(headers or {})
        if self.token:
            hdrs.setdefault("Authorization", f"Bearer {self.token}")
        req = urllib.request.Request(self.base_url + path, headers=hdrs, method="GET")
        return self._opener.open(req, timeout=timeout)
