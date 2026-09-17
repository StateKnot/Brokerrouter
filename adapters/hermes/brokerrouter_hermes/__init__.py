"""Brokerrouter's fail-closed Hermes 0.21.1 execution gate."""

from .gate import (
    GovernanceHalt,
    bind_gateway_agent,
    gate_for_agent,
    reject_governed_proxy,
)
from .integrity import verify_runtime_installation

__all__ = [
    "GovernanceHalt",
    "bind_gateway_agent",
    "gate_for_agent",
    "reject_governed_proxy",
    "verify_runtime_installation",
]
