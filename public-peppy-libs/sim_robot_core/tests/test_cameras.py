"""Tests for the camera plumbing: parsing a model entry's cameras, z16
conversion, and pacing."""

import struct
from pathlib import Path

import numpy as np
import pyjson5
import pytest

from sim_robot_core.cameras import (
    DepthSpec,
    FrameIdCounter,
    FramePacer,
    depth_to_z16,
    parse_cameras,
)
from sim_robot_core.models import shipped_entry


def load_camera_configs(path: Path):
    """The cameras of an entry written to `path`."""
    return parse_cameras(str(path), pyjson5.loads(path.read_text())["cameras"])


class TestParseCameras:
    def test_the_openarm_v2_rig_parses(self):
        configs = shipped_entry("openarm_v2").cameras
        assert [c.name for c in configs] == [
            "wrist_left",
            "wrist_right",
            "chest",
        ]
        by_name = {c.name: c for c in configs}
        for wrist in ("wrist_left", "wrist_right"):
            assert (by_name[wrist].width, by_name[wrist].height) == (960, 600)
            assert by_name[wrist].depth is None
            assert by_name[wrist].fps == 15
        chest = by_name["chest"]
        assert (chest.width, chest.height) == (1280, 720)
        assert chest.parent_link == "openarm_body_link0"
        # The ZED Mini's left lens front as Waldo's head camera derivation
        # reads it off Enactic's CAD, looking 62 degrees below the horizon.
        assert chest.pos == pytest.approx((0.0792, 0.0315, 0.7941))
        assert chest.quat_wxyz == pytest.approx((0.6861027, 0.1710647, -0.1710647, -0.6861027))
        assert chest.depth is not None
        assert (chest.depth.width, chest.depth.height) == (640, 360)

    def _write_config(self, tmp_path, body: str) -> Path:
        path = tmp_path / "cameras.json5"
        path.write_text(body)
        return path

    def _entry(self, **overrides) -> str:
        fields = {
            "name": '"cam"',
            "parent_link": '"link"',
            "pos": "[0, 0, 0]",
            "quat_wxyz": "[1, 0, 0, 0]",
            "fovy_deg": "60",
            "color": "{ width: 8, height: 4 }",
            "fps": "15",
        }
        fields.update(overrides)
        return "{" + ", ".join(f"{k}: {v}" for k, v in fields.items()) + "}"

    def test_unknown_camera_key_rejected(self, tmp_path):
        """A misspelled optional key must fail, not silently change the stream:
        `dpeth` would otherwise yield a colour-only camera where the file
        plainly describes an rgbd one."""
        path = self._write_config(
            tmp_path,
            "{ cameras: ["
            + self._entry(dpeth="{ width: 4, height: 2, min_depth_m: 0.1, max_range_m: 10 }")
            + "] }",
        )
        with pytest.raises(RuntimeError, match="unknown key"):
            load_camera_configs(path)

    def test_unknown_color_key_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path,
            "{ cameras: [" + self._entry(color="{ width: 8, height: 4, fps: 15 }") + "] }",
        )
        with pytest.raises(RuntimeError, match="unknown key"):
            load_camera_configs(path)

    def test_unknown_depth_key_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path,
            "{ cameras: ["
            + self._entry(
                depth="{ width: 4, height: 2, min_depth_m: 0.1, max_range_m: 10, units: 0.001 }"
            )
            + "] }",
        )
        with pytest.raises(RuntimeError, match="unknown key"):
            load_camera_configs(path)

    def test_duplicate_names_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry()}, {self._entry()}] }}"
        )
        with pytest.raises(RuntimeError, match="duplicate"):
            load_camera_configs(path)

    def test_a_model_with_no_camera_lists_none(self, tmp_path):
        path = self._write_config(tmp_path, "{ cameras: [] }")
        assert load_camera_configs(path) == ()

    def test_cameras_that_are_not_a_list_are_rejected(self):
        with pytest.raises(RuntimeError, match="'cameras' must be a list"):
            parse_cameras("entry", {"name": "cam"})

    def test_unnormalized_quat_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path,
            f"{{ cameras: [{self._entry(quat_wxyz='[1, 1, 0, 0]')}] }}",
        )
        with pytest.raises(RuntimeError, match="norm"):
            load_camera_configs(path)

    def test_nonfinite_pos_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry(pos='[0, 0, NaN]')}] }}"
        )
        with pytest.raises(RuntimeError, match="finite"):
            load_camera_configs(path)

    @pytest.mark.parametrize("fovy", ["0", "180", "-10"])
    def test_fovy_out_of_range_rejected(self, tmp_path, fovy):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry(fovy_deg=fovy)}] }}"
        )
        with pytest.raises(RuntimeError, match="fovy"):
            load_camera_configs(path)

    @pytest.mark.parametrize("fps", ["0", "-1", "1.5", "256", "true"])
    def test_bad_fps_rejected(self, tmp_path, fps):
        path = self._write_config(tmp_path, f"{{ cameras: [{self._entry(fps=fps)}] }}")
        with pytest.raises(RuntimeError, match="fps"):
            load_camera_configs(path)

    def test_bool_fovy_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry(fovy_deg='true')}] }}"
        )
        with pytest.raises(RuntimeError, match="fovy"):
            load_camera_configs(path)

    def test_bool_pos_component_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry(pos='[0, true, 0]')}] }}"
        )
        with pytest.raises(RuntimeError, match="finite"):
            load_camera_configs(path)

    def test_non_object_entry_rejected(self, tmp_path):
        path = self._write_config(tmp_path, "{ cameras: [7] }")
        with pytest.raises(RuntimeError, match="object"):
            load_camera_configs(path)

    def test_non_object_depth_rejected(self, tmp_path):
        path = self._write_config(
            tmp_path, f"{{ cameras: [{self._entry(depth='7')}] }}"
        )
        with pytest.raises(RuntimeError, match="depth must be an object"):
            load_camera_configs(path)

    def test_bool_depth_range_rejected(self, tmp_path):
        entry = self._entry(
            depth="{ width: 4, height: 2, min_depth_m: true, max_range_m: 1.0 }"
        )
        path = self._write_config(tmp_path, f"{{ cameras: [{entry}] }}")
        with pytest.raises(RuntimeError, match="depth range"):
            load_camera_configs(path)

    def test_inverted_depth_range_rejected(self, tmp_path):
        entry = self._entry(
            depth="{ width: 4, height: 2, min_depth_m: 2.0, max_range_m: 1.0 }"
        )
        path = self._write_config(tmp_path, f"{{ cameras: [{entry}] }}")
        with pytest.raises(RuntimeError, match="depth range"):
            load_camera_configs(path)

    def test_fps_upper_bound_accepted(self, tmp_path):
        path = self._write_config(tmp_path, f"{{ cameras: [{self._entry(fps='255')}] }}")
        (config,) = load_camera_configs(path)
        assert config.fps == 255

    @pytest.mark.parametrize(
        "depth_range",
        [
            "min_depth_m: 0.1, max_range_m: 100.0",
            "min_depth_m: NaN, max_range_m: 1.0",
            "min_depth_m: 0.1, max_range_m: Infinity",
        ],
    )
    def test_depth_range_outside_wire_ceiling_rejected(self, tmp_path, depth_range):
        path = self._write_config(
            tmp_path,
            f"{{ cameras: [{self._entry(depth=f'{{ width: 4, height: 2, {depth_range} }}')}] }}",
        )
        with pytest.raises(RuntimeError, match="wire ceiling"):
            load_camera_configs(path)

    @pytest.mark.parametrize("dims", ["width: 3, height: 2", "width: 4, height: 1"])
    def test_depth_grid_must_subsample_color_grid(self, tmp_path, dims):
        path = self._write_config(
            tmp_path,
            f"{{ cameras: [{self._entry(depth=f'{{ {dims}, min_depth_m: 0.1, max_range_m: 1.0 }}')}] }}",
        )
        with pytest.raises(RuntimeError, match="subsample"):
            load_camera_configs(path)


class TestDepthSpec:
    def test_range_beyond_wire_ceiling_rejected(self):
        with pytest.raises(ValueError, match="wire ceiling"):
            DepthSpec(width=4, height=2, min_depth_m=0.1, max_range_m=65.536)

    def test_nonpositive_min_rejected(self):
        with pytest.raises(ValueError, match="wire ceiling"):
            DepthSpec(width=4, height=2, min_depth_m=0.0, max_range_m=1.0)

    def test_inverted_range_rejected(self):
        with pytest.raises(ValueError, match="wire ceiling"):
            DepthSpec(width=4, height=2, min_depth_m=2.0, max_range_m=1.0)


class TestDepthToZ16:
    _SPEC = DepthSpec(width=3, height=1, min_depth_m=0.4, max_range_m=65.535)

    def _decode(self, payload: bytes) -> list[int]:
        return list(struct.unpack(f"<{len(payload) // 2}H", payload))

    def test_in_range_converts_to_little_endian_mm(self):
        payload = depth_to_z16(np.array([[0.5, 1.0, 2.5]], dtype=np.float32), self._SPEC)
        assert self._decode(payload) == [500, 1000, 2500]

    def test_out_of_range_and_nonfinite_become_zero(self):
        depth = np.array([[0.1, np.nan, np.inf]], dtype=np.float32)
        assert self._decode(depth_to_z16(depth, self._SPEC)) == [0, 0, 0]

    def test_rounds_to_nearest_millimeter(self):
        depth = np.array([[1.2349, 1.2351, 0.4004]], dtype=np.float64)
        assert self._decode(depth_to_z16(depth, self._SPEC)) == [1235, 1235, 400]

    def test_exact_bounds_encode(self):
        depth = np.array([[0.4, 65.535, 65.0]], dtype=np.float64)
        assert self._decode(depth_to_z16(depth, self._SPEC)) == [400, 65535, 65000]

    def test_sub_millimeter_clips_to_one(self):
        spec = DepthSpec(width=1, height=1, min_depth_m=0.0002, max_range_m=1.0)
        depth = np.array([[0.0004]], dtype=np.float64)
        assert self._decode(depth_to_z16(depth, spec)) == [1]

    def test_shape_mismatch_rejected(self):
        with pytest.raises(ValueError, match="shape"):
            depth_to_z16(np.zeros((2, 3), dtype=np.float32), self._SPEC)


class TestFrameIdCounter:
    def test_wraps_at_u32(self):
        counter = FrameIdCounter()
        counter._next = 0xFFFFFFFF
        assert counter.next() == 0xFFFFFFFF
        assert counter.next() == 0


class TestFramePacer:
    def test_remaining_wait_does_not_initialize_the_schedule(self):
        pacer = FramePacer(10)
        assert pacer.seconds_until_due(0.0) == 0.0
        assert pacer.seconds_until_due(100.0) == 0.0
        assert pacer.take_if_due(100.0)
        assert pacer.seconds_until_due(100.0) == pytest.approx(0.1)

    def test_remaining_wait_does_not_claim_or_advance_a_deadline(self):
        pacer = FramePacer(10)
        assert pacer.take_if_due(100.0)
        for _ in range(2):
            assert pacer.seconds_until_due(100.04) == pytest.approx(0.06)
            assert pacer.seconds_until_due(100.1) == 0.0
            assert pacer.seconds_until_due(105.0) == 0.0
        assert pacer.take_if_due(100.1)
        assert pacer.seconds_until_due(100.1) == pytest.approx(0.1)

    def test_first_call_is_due(self):
        pacer = FramePacer(10)
        assert pacer.take_if_due(100.0)
        assert not pacer.take_if_due(100.05)
        assert pacer.take_if_due(100.1)

    def test_stall_resyncs_instead_of_bursting(self):
        pacer = FramePacer(10)
        assert pacer.take_if_due(100.0)
        assert pacer.take_if_due(105.0)
        # After the resync the next frame is one period out, not a burst of
        # 50 missed frames.
        assert not pacer.take_if_due(105.05)
        assert pacer.take_if_due(105.1)

    def test_ticks_jittered_around_the_period_are_all_taken(self):
        # A 60 Hz request over a 60 fps loop whose frames alternate 16.2 and
        # 17.2 ms: one early tick skips, and the grid must absorb the late
        # one that follows rather than resync onto it, or the pair locks
        # into taking every other tick.
        pacer = FramePacer(60)
        ticks = [100.0 + (i // 2) * 0.0334 + (i % 2) * 0.0162 for i in range(600)]
        taken = [pacer.take_if_due(t) for t in ticks]
        assert sum(taken) >= 598

    def test_ticks_slightly_faster_than_the_period_skip_at_most_one_in_a_row(self):
        pacer = FramePacer(60)
        taken = [pacer.take_if_due(100.0 + i * 0.0165) for i in range(600)]
        assert sum(taken) >= 594
        assert all(a or b for a, b in zip(taken, taken[1:]))
