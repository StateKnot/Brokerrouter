from __future__ import annotations

from dataclasses import dataclass, field
from contextlib import closing
import json
from pathlib import Path
import re
import ssl
import stat
from typing import Any, Protocol
from urllib.parse import quote

import httpx
import certifi

from .config import ConfigurationError


MAX_RESPONSE_BYTES = 1024 * 1024


def _gateway_tls(ca_file: Path | None) -> ssl.SSLContext | bool:
    if ca_file is None:
        return True
    try:
        if not ca_file.is_absolute() or not stat.S_ISREG(ca_file.stat().st_mode):
            raise ValueError("invalid file type")
        with ca_file.open("rb") as handle:
            content = handle.read(1024 * 1024 + 1)
        if not content or len(content) > 1024 * 1024:
            raise ValueError("invalid file size")
        text = content.decode("ascii")
        certificates = re.findall(
            r"-----BEGIN CERTIFICATE-----[\r\n]+[A-Za-z0-9+/=\r\n]+-----END CERTIFICATE-----",
            text,
        )
        rest = text
        for certificate in certificates:
            rest = rest.replace(certificate, "", 1)
        if not 1 <= len(certificates) <= 128 or rest.strip(" \t\r\n\v\f"):
            raise ValueError("expected certificate-only PEM bundle")
        # Match HTTPX's trust_env=False public roots, then add this purpose's CA.
        context = ssl.create_default_context(cafile=certifi.where())
        for certificate in certificates:
            context.load_verify_locations(cadata=certificate)
        return context
    except (OSError, UnicodeError, ValueError, ssl.SSLError) as exc:
        raise ConfigurationError("invalid Hermes gateway CA bundle") from exc


class TransportFailure(RuntimeError):
    pass


class GatewayFailure(RuntimeError):
    def __init__(self, status: int, code: str) -> None:
        super().__init__(f"Brokerrouter returned HTTP {status} ({code})")
        self.status = status
        self.code = code


class Transport(Protocol):
    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]: ...


@dataclass
class HttpTransport:
    base_url: str
    virtual_key: str = field(repr=False)
    timeout_seconds: float
    ca_file: Path | None = None

    def __post_init__(self) -> None:
        self._client = httpx.Client(
            base_url=self.base_url,
            headers={"authorization": f"Bearer {self.virtual_key}", "accept": "application/json", "accept-encoding": "identity"},
            timeout=httpx.Timeout(self.timeout_seconds),
            follow_redirects=False,
            trust_env=False,
            verify=_gateway_tls(self.ca_file),
            limits=httpx.Limits(max_connections=16, max_keepalive_connections=8),
        )

    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
        try:
            request = self._client.build_request(method, path, json=body)
            response = self._client.send(request, stream=True)
            with closing(response):
                if response.headers.get("content-encoding", "").strip().lower() not in {"", "identity"}:
                    raise TransportFailure("Brokerrouter response must use identity encoding")
                raw = bytearray()
                for chunk in response.iter_raw():
                    raw.extend(chunk)
                    if len(raw) > MAX_RESPONSE_BYTES:
                        raise TransportFailure("Brokerrouter response exceeded 1 MiB")
                content_type = response.headers.get("content-type", "").split(";", 1)[0].strip().lower()
                if content_type != "application/json":
                    raise TransportFailure("Brokerrouter response was not JSON")
                try:
                    parsed = json.loads(bytes(raw).decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    raise TransportFailure("Brokerrouter response contained invalid JSON") from exc
        except httpx.HTTPError as exc:
            raise TransportFailure("Brokerrouter transport failed") from exc
        if not isinstance(parsed, dict):
            raise TransportFailure("Brokerrouter response must be an object")
        if not 200 <= response.status_code < 300:
            error = parsed.get("error")
            code = error.get("code") if isinstance(error, dict) else None
            raise GatewayFailure(response.status_code, code if isinstance(code, str) else "unknown_error")
        return parsed


class GovernanceClient:
    def __init__(self, transport: Transport) -> None:
        self._transport = transport

    def check(self, body: dict[str, Any]) -> dict[str, Any]:
        return self._transport.request("POST", "/v1/governance/checks", body)

    def bind_turn(self, turn_id: str, body: dict[str, Any]) -> dict[str, Any]:
        path = f"/v1/governance/turns/{quote(turn_id, safe='')}/bind"
        return self._transport.request("POST", path, body)

    def claim(self, operation_id: str, token: str, payload_hash: str) -> dict[str, Any]:
        path = f"/v1/governance/operations/{quote(operation_id, safe='')}/claim"
        return self._transport.request("POST", path, {"token": token, "payload_hash": payload_hash})

    def receipt(self, operation_id: str, token: str, outcome: str, evidence_ref: str) -> dict[str, Any]:
        path = f"/v1/governance/operations/{quote(operation_id, safe='')}/receipt"
        return self._transport.request(
            "POST", path, {"token": token, "outcome": outcome, "evidence_ref": evidence_ref}
        )

    def record_tool_result(
        self, operation_id: str, token: str, evidence_ref: str, text: str
    ) -> dict[str, Any]:
        return self._transport.request(
            "POST",
            "/v1/governance/tool-results",
            {
                "input_operation_id": operation_id,
                "claim_token": token,
                "evidence_ref": evidence_ref,
                "text": text,
            },
        )

    def release_tool_result(self, result_id: str) -> dict[str, Any]:
        # Releasing can drive a paid semantic check; status reads must not do so.
        path = f"/v1/governance/tool-results/{quote(result_id, safe='')}/release"
        return self._transport.request("POST", path, {})
