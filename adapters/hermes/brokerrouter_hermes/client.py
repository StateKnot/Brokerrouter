from __future__ import annotations

from dataclasses import dataclass, field
import json
from typing import Any, Protocol
from urllib.parse import quote

import httpx


MAX_RESPONSE_BYTES = 1024 * 1024


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

    def __post_init__(self) -> None:
        self._client = httpx.Client(
            base_url=self.base_url,
            headers={"authorization": f"Bearer {self.virtual_key}", "accept": "application/json"},
            timeout=httpx.Timeout(self.timeout_seconds),
            follow_redirects=False,
            trust_env=False,
            limits=httpx.Limits(max_connections=16, max_keepalive_connections=8),
        )

    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
        try:
            request = self._client.build_request(method, path, json=body)
            response = self._client.send(request, stream=True)
            with response:
                raw = bytearray()
                for chunk in response.iter_bytes():
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

    def claim(self, operation_id: str, token: str, payload_hash: str) -> dict[str, Any]:
        path = f"/v1/governance/operations/{quote(operation_id, safe='')}/claim"
        return self._transport.request("POST", path, {"token": token, "payload_hash": payload_hash})

    def receipt(self, operation_id: str, token: str, outcome: str, evidence_ref: str) -> dict[str, Any]:
        path = f"/v1/governance/operations/{quote(operation_id, safe='')}/receipt"
        return self._transport.request(
            "POST", path, {"token": token, "outcome": outcome, "evidence_ref": evidence_ref}
        )
