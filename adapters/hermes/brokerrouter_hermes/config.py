from __future__ import annotations

from dataclasses import dataclass, field
import os
from pathlib import Path
from urllib.parse import urlsplit


class ConfigurationError(RuntimeError):
    pass


def _bounded_float(env: dict[str, str], name: str, default: float, low: float, high: float) -> float:
    raw = env.get(name, str(default))
    try:
        value = float(raw)
    except ValueError as exc:
        raise ConfigurationError(f"{name} must be a number") from exc
    if not low <= value <= high:
        raise ConfigurationError(f"{name} must be between {low} and {high}")
    return value


@dataclass(frozen=True)
class Config:
    base_url: str
    virtual_key: str = field(repr=False)
    state_db: Path
    state_key: bytes = field(repr=False)
    request_timeout_seconds: float
    approval_poll_seconds: float
    approval_wait_seconds: float

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> Config | None:
        values = dict(os.environ if env is None else env)
        mode = values.get("BROKERROUTER_HERMES_GOVERNANCE", "off").strip().lower()
        if mode == "off":
            return None
        if mode != "required":
            raise ConfigurationError("BROKERROUTER_HERMES_GOVERNANCE must be off or required")

        required = {
            name: values.get(name, "").strip()
            for name in (
                "BROKERROUTER_BASE_URL",
                "BROKERROUTER_VIRTUAL_KEY",
                "BROKERROUTER_HERMES_STATE_DB",
                "BROKERROUTER_HERMES_STATE_KEY",
            )
        }
        missing = [name for name, value in required.items() if not value]
        if missing:
            raise ConfigurationError(f"missing required setting: {', '.join(missing)}")

        url = urlsplit(required["BROKERROUTER_BASE_URL"])
        allow_http = values.get("BROKERROUTER_HERMES_ALLOW_LOOPBACK_HTTP", "false").lower() == "true"
        loopback = url.hostname in {"127.0.0.1", "::1", "localhost"}
        if (
            url.scheme not in ({"https"} | ({"http"} if allow_http and loopback else set()))
            or not url.hostname
            or url.username
            or url.password
            or url.path not in {"", "/"}
            or url.query
            or url.fragment
        ):
            raise ConfigurationError("Brokerrouter URL must be HTTPS without credentials/query/fragment")
        virtual_key = required["BROKERROUTER_VIRTUAL_KEY"]
        if (
            not virtual_key.startswith("sk_br_")
            or len(virtual_key) != 70
            or any(character not in "0123456789abcdef" for character in virtual_key[6:])
        ):
            raise ConfigurationError("BROKERROUTER_VIRTUAL_KEY must be a Brokerrouter virtual key")

        state_db = Path(required["BROKERROUTER_HERMES_STATE_DB"])
        if not state_db.is_absolute() or state_db.name in {"", ".", ".."}:
            raise ConfigurationError("BROKERROUTER_HERMES_STATE_DB must be an absolute file path")
        try:
            key = bytes.fromhex(required["BROKERROUTER_HERMES_STATE_KEY"])
        except ValueError as exc:
            raise ConfigurationError("BROKERROUTER_HERMES_STATE_KEY must be hex") from exc
        if len(key) != 32:
            raise ConfigurationError("BROKERROUTER_HERMES_STATE_KEY must encode exactly 32 bytes")

        return cls(
            base_url=required["BROKERROUTER_BASE_URL"].rstrip("/"),
            virtual_key=required["BROKERROUTER_VIRTUAL_KEY"],
            state_db=state_db,
            state_key=key,
            request_timeout_seconds=_bounded_float(
                values, "BROKERROUTER_HERMES_REQUEST_TIMEOUT_SECONDS", 10.0, 1.0, 30.0
            ),
            approval_poll_seconds=_bounded_float(
                values, "BROKERROUTER_HERMES_APPROVAL_POLL_SECONDS", 2.0, 0.25, 30.0
            ),
            approval_wait_seconds=_bounded_float(
                values, "BROKERROUTER_HERMES_APPROVAL_WAIT_SECONDS", 300.0, 30.0, 3600.0
            ),
        )
