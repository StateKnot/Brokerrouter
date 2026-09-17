"""Brokerrouter's fail-closed Hermes 0.21.1 execution gate."""

from .gate import GovernanceHalt, gate_for_agent

__all__ = ["GovernanceHalt", "gate_for_agent"]
