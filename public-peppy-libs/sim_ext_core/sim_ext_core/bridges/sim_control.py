"""Sim control bridge — pause, resume, and step the sim over raw topics."""

from __future__ import annotations

import json
import logging
from abc import ABC, abstractmethod
from typing import Any

from sim_ext_core.base import BridgePlugin

logger = logging.getLogger(__name__)

# Idempotent services: latest queued wins (a stale pause is harmless if
# a newer one arrived). Non-idempotent services carry distinct payloads;
# every queued request must be processed.
_IDEMPOTENT_SERVICES = ("reset_sim", "pause_sim")
_NON_IDEMPOTENT_SERVICES = ("step_sim", "set_joint_positions")
_SERVICES = _IDEMPOTENT_SERVICES + _NON_IDEMPOTENT_SERVICES
_TOPIC_PREFIX = "sim_ctrl_"
_REQ_SUFFIX = "_req"
_RES_SUFFIX = "_res"
_QOS = "standard"


class SimControlInterface(ABC):
    """Engine hooks the sim launcher provides for pause/resume/step."""

    @abstractmethod
    def reset(self) -> dict: ...

    @abstractmethod
    def pause(self, paused: bool) -> dict: ...

    @abstractmethod
    def step(self, num_steps: int) -> dict: ...

    @abstractmethod
    def set_joint_positions(self, arm: str, positions: list[float]) -> dict: ...


class SimControlBridge(BridgePlugin):
    """Applies pause/resume/step_sim requests against the SimControlInterface."""

    def __init__(
        self,
        sim_control: SimControlInterface,
        config: Any,
        source_node: str = "sim_bridge",
    ) -> None:
        self._impl = sim_control
        self._node_name: str = config.node_name
        self._source_node = source_node
        self._ready = True

    def setup(self) -> bool:
        return True

    def teardown(self) -> None:
        pass

    @property
    def is_ready(self) -> bool:
        return self._ready

    def subscriptions(self) -> list[tuple[str, str, str]]:
        return [
            (self._source_node, f"{_TOPIC_PREFIX}{svc}{_REQ_SUFFIX}", _QOS)
            for svc in _SERVICES
        ]

    def on_step(self, step: int, io: Any) -> None:
        for svc in _IDEMPOTENT_SERVICES:
            topic = f"{_TOPIC_PREFIX}{svc}{_REQ_SUFFIX}"
            raw = io.get_latest(self._source_node, topic)
            if raw is not None:
                self._handle_request(svc, raw, io)

        for svc in _NON_IDEMPOTENT_SERVICES:
            topic = f"{_TOPIC_PREFIX}{svc}{_REQ_SUFFIX}"
            for raw in io.get_all(self._source_node, topic):
                self._handle_request(svc, raw, io)

    def _handle_request(self, svc: str, raw: bytes, io: Any) -> None:
        try:
            request = json.loads(raw)
        except (json.JSONDecodeError, TypeError, ValueError) as exc:
            logger.warning(f"sim_control: malformed JSON on {svc}: {exc}")
            response = {"success": False, "message": f"malformed JSON: {exc}"}
        else:
            try:
                response = self._dispatch(svc, request)
            except Exception as exc:
                logger.error(f"sim_control: unhandled error in {svc}: {exc}")
                response = {"success": False, "message": str(exc)}
        io.emit(
            self._node_name,
            f"{_TOPIC_PREFIX}{svc}{_RES_SUFFIX}",
            _QOS,
            json.dumps(response).encode(),
        )

    def _dispatch(self, service: str, request: dict) -> dict:
        if service == "reset_sim":
            return self._impl.reset()
        if service == "pause_sim":
            return self._impl.pause(bool(request.get("paused", True)))
        if service == "step_sim":
            return self._impl.step(int(request.get("num_steps", 1)))
        if service == "set_joint_positions":
            return self._impl.set_joint_positions(
                str(request.get("arm", "")),
                list(request.get("positions", [])),
            )
        raise ValueError(f"unknown service: {service}")
