"""Actuator command bridge — applies set_ctrl payloads to the sim."""

from __future__ import annotations

import json
import logging
from typing import Any

from sim_ext_core.base import BridgePlugin

logger = logging.getLogger(__name__)

_QOS = "standard"


class ActuatorCtrlBridge(BridgePlugin):
    """Subscribes set_ctrl_* payloads and writes actuator targets into the sim."""

    def __init__(self, actuator_ctrl: Any, _config: Any, entry: Any) -> None:
        self._actuator_ctrl = actuator_ctrl
        self._source_node: str = entry.source_node
        self._topic: str = entry.topic

    def setup(self) -> bool:
        return self._actuator_ctrl.setup()

    def teardown(self) -> None:
        self._actuator_ctrl.teardown()

    def subscriptions(self) -> list[tuple[str, str, str]]:
        return [(self._source_node, self._topic, _QOS)]

    def on_step(self, step: int, io: Any) -> None:
        raw = io.get_latest(self._source_node, self._topic)
        if raw is None:
            return
        try:
            msg = json.loads(raw)
        except (json.JSONDecodeError, TypeError) as exc:
            logger.warning(f"set_ctrl: malformed JSON on {self._topic}: {exc}")
            return
        values = msg.get("actuator_values") if isinstance(msg, dict) else None
        if not isinstance(values, dict):
            logger.warning(
                f"set_ctrl: payload missing actuator_values dict on {self._topic}"
            )
            return
        # Optional velocity setpoints (trajectory feedforward) — older
        # publishers omit them and the engine wrapper defaults them to zero.
        velocities = msg.get("velocity_values")
        self._actuator_ctrl.write_targets(values, velocities)

    @property
    def is_ready(self) -> bool:
        return self._actuator_ctrl.is_ready
