"""One entry per model: what it parses, what it checks, and how an engine's
own entries pair with the ones this package ships."""

from __future__ import annotations

import pytest

from sim_robot_core.models import (
    CLOSED_AT_LOWER_LIMIT,
    CLOSED_AT_ZERO,
    Models,
    finger_span,
    parse_entry,
    shipped_entry,
    shipped_models,
)
from sim_robot_core.pairs import Held

OPENARM_LIMBS = Held(
    arms=frozenset({"left_arm", "right_arm"}),
    grippers=frozenset({"left_gripper", "right_gripper"}),
)
SO101_LIMBS = Held(arms=frozenset({"arm"}), grippers=frozenset({"gripper"}))


def entry(**overrides):
    raw = {
        "arms": [{"name": "arm", "joints": ["j1", "j2"]}],
        "grippers": [{"name": "gripper", "fingers": ["f1"], "closed_at": "zero"}],
    }
    raw.update(overrides)
    return parse_entry("test_arm", "test_arm.json5", raw)


class TestShippedEntries:
    def test_the_package_ships_both_openarms_and_the_so101(self):
        assert shipped_models() == ["openarm_v1", "openarm_v2", "so101"]

    @pytest.mark.parametrize("model", ["openarm_v1", "openarm_v2"])
    def test_an_openarm_names_its_limbs_as_the_robot_answers_to_them(self, model):
        openarm = shipped_entry(model)
        assert openarm.arm_names() == ["left_arm", "right_arm"]
        assert openarm.arm_joint_counts() == [7, 7]
        assert openarm.gripper_names() == ["left_gripper", "right_gripper"]
        assert openarm.start_posture == {}

    def test_only_the_v2_carries_the_camera_rig(self):
        assert shipped_entry("openarm_v1").cameras == ()
        v2 = shipped_entry("openarm_v2")
        assert v2.rgb_camera_names() == ["wrist_left", "wrist_right"]
        assert v2.rgbd_camera_names() == ["chest"]

    def test_the_so101_is_one_arm_one_jaw_and_a_front_camera(self):
        so101 = shipped_entry("so101")
        assert (so101.arm_names(), so101.arm_joint_counts()) == (["arm"], [5])
        assert so101.gripper_names() == ["gripper"]
        assert so101.grippers[0].closed_at == CLOSED_AT_LOWER_LIMIT
        assert (so101.rgb_camera_names(), so101.rgbd_camera_names()) == (["front"], [])

    def test_a_model_with_no_entry_names_the_ones_that_have_one(self):
        with pytest.raises(ValueError, match="the entries are openarm_v1, openarm_v2, so101"):
            shipped_entry("aloha")


class TestTheMatch:
    def test_every_limb_and_nothing_else_is_a_match(self):
        assert shipped_entry("openarm_v2").mismatch(OPENARM_LIMBS) is None
        assert shipped_entry("so101").mismatch(SO101_LIMBS) is None

    def test_a_camera_may_go_unpaired(self):
        rig = Held(
            arms=OPENARM_LIMBS.arms,
            grippers=OPENARM_LIMBS.grippers,
            rgb_cameras=frozenset({"wrist_left"}),
        )
        assert shipped_entry("openarm_v2").mismatch(rig) is None

    def test_a_missing_limb_names_both_lists(self):
        one_arm = Held(arms=frozenset({"left_arm"}), grippers=OPENARM_LIMBS.grippers)
        assert shipped_entry("openarm_v2").mismatch(one_arm) == (
            "its pairs name arms ['left_arm'], grippers ['left_gripper', 'right_gripper'], "
            "rgb_cameras [], rgbd_cameras [], and the model openarm_v2 has arms ['left_arm', "
            "'right_arm'], grippers ['left_gripper', 'right_gripper'], rgb_cameras "
            "['wrist_left', 'wrist_right'], rgbd_cameras ['chest']"
        )

    def test_an_so101_paired_as_an_openarm_matches_nothing(self):
        so101 = shipped_entry("so101")
        assert not so101.holds_every_limb(OPENARM_LIMBS)
        assert so101.foreign(OPENARM_LIMBS) == OPENARM_LIMBS
        assert "the model so101 has arms ['arm'], grippers ['gripper']" in so101.mismatch(OPENARM_LIMBS)

    def test_a_limb_the_model_lacks_is_foreign_even_beside_every_limb_it_has(self):
        extra = Held(arms=frozenset({"arm", "left_arm"}), grippers=SO101_LIMBS.grippers)
        so101 = shipped_entry("so101")
        assert so101.holds_every_limb(extra)
        assert so101.foreign(extra) == Held(arms=frozenset({"left_arm"}))
        assert so101.mismatch(extra) is not None

    def test_a_camera_of_the_wrong_kind_is_foreign(self):
        depth_front = Held(
            arms=SO101_LIMBS.arms, grippers=SO101_LIMBS.grippers, rgbd_cameras=frozenset({"front"})
        )
        assert shipped_entry("so101").foreign(depth_front) == Held(rgbd_cameras=frozenset({"front"}))

    def test_a_camera_on_a_model_with_no_rig_is_foreign(self):
        v1_rig = Held(
            arms=OPENARM_LIMBS.arms,
            grippers=OPENARM_LIMBS.grippers,
            rgbd_cameras=frozenset({"chest"}),
        )
        assert shipped_entry("openarm_v1").mismatch(v1_rig) is not None


class TestParsing:
    def test_an_entry_parses_its_limbs_in_order(self):
        parsed = entry()
        assert parsed.joints() == ["j1", "j2", "f1"]
        assert (parsed.arm_joints(), parsed.finger_joints()) == (["j1", "j2"], ["f1"])

    def test_an_unknown_key_is_refused(self):
        with pytest.raises(RuntimeError, match="unknown key\\(s\\) \\['arm_gains'\\]"):
            entry(arm_gains={})

    def test_an_unknown_limb_key_is_refused(self):
        with pytest.raises(RuntimeError, match="arm 'arm' has unknown key\\(s\\) \\['gains'\\]"):
            entry(arms=[{"name": "arm", "joints": ["j1"], "gains": {}}])

    def test_a_model_with_no_limb_is_refused(self):
        with pytest.raises(RuntimeError, match="at least one limb"):
            entry(arms=[], grippers=[])

    def test_two_limbs_under_one_name_are_refused(self):
        with pytest.raises(RuntimeError, match="limb names must be unique"):
            entry(grippers=[{"name": "arm", "fingers": ["f1"], "closed_at": "zero"}])

    def test_a_joint_in_two_limbs_is_refused(self):
        with pytest.raises(RuntimeError, match="a joint belongs to one limb"):
            entry(grippers=[{"name": "gripper", "fingers": ["j1"], "closed_at": "zero"}])

    def test_an_arm_with_no_joint_is_refused(self):
        with pytest.raises(RuntimeError, match="arm 'arm' joints must be a non-empty list"):
            entry(arms=[{"name": "arm", "joints": []}])

    def test_a_gripper_must_say_where_it_closes(self):
        with pytest.raises(RuntimeError, match="closed_at None is not one of"):
            entry(grippers=[{"name": "gripper", "fingers": ["f1"]}])

    def test_a_start_posture_names_the_models_own_joints(self):
        assert entry(start_posture={"j1": 1, "j2": -0.5}).start_posture == {"j1": 1.0, "j2": -0.5}
        with pytest.raises(RuntimeError, match="names joints no limb moves: \\['j9'\\]"):
            entry(start_posture={"j9": 0.0})

    @pytest.mark.parametrize("position", [True, "0.1", float("nan"), float("inf")])
    def test_a_start_position_is_a_finite_number(self, position):
        with pytest.raises(RuntimeError, match="start_posture of 'j1'"):
            entry(start_posture={"j1": position})


class TestFingerSpan:
    def test_a_finger_closed_at_zero_opens_toward_the_far_end_of_its_travel(self):
        prismatic = finger_span("f", 0.0, 0.044, CLOSED_AT_ZERO)
        assert (prismatic.closed, prismatic.travel) == (0.0, 0.044)
        mirrored = finger_span("f", -0.7854, 0.0, CLOSED_AT_ZERO)
        assert mirrored.position(1.0) == pytest.approx(-0.7854)

    def test_symmetric_slack_around_the_travel_cancels(self):
        assert finger_span("f", -0.01, 0.054, CLOSED_AT_ZERO).travel == pytest.approx(0.044)

    def test_a_finger_closed_at_its_lower_limit_travels_its_whole_range(self):
        jaw = finger_span("gripper", -0.174533, 1.74533, CLOSED_AT_LOWER_LIMIT)
        assert jaw.position(0.0) == pytest.approx(-0.174533)
        assert jaw.position(1.0) == pytest.approx(1.74533)
        assert jaw.opening(jaw.position(0.25)) == pytest.approx(0.25)

    def test_a_range_that_excludes_the_closed_pose_is_refused(self):
        with pytest.raises(RuntimeError, match="does not contain the closed pose"):
            finger_span("f", 0.01, 0.044, CLOSED_AT_ZERO)

    @pytest.mark.parametrize(
        "lower, upper, closed_at",
        [(-0.02, 0.02, CLOSED_AT_ZERO), (0.3, 0.3, CLOSED_AT_LOWER_LIMIT)],
    )
    def test_a_finger_with_no_travel_is_refused(self, lower, upper, closed_at):
        with pytest.raises(RuntimeError, match="no usable travel"):
            finger_span("f", lower, upper, closed_at)

    def test_an_unknown_rule_is_refused(self):
        with pytest.raises(RuntimeError, match="closed_at 'upper_limit' is not one of"):
            finger_span("f", 0.0, 1.0, "upper_limit")


class TestAnEnginesModels:
    def test_an_engine_stands_the_models_it_has_an_entry_for(self, tmp_path):
        (tmp_path / "so101.json5").write_text("{ scene: 'so101/scene.xml' }")
        (tmp_path / "openarm_v2.json5").write_text("{ scene: 'v2.xml', head_camera: true }")
        models = Models.read(tmp_path)
        assert models.names() == ["openarm_v2", "so101"]
        assert models.of("so101").engine == {"scene": "so101/scene.xml"}
        assert models.of("so101").entry == shipped_entry("so101")

    def test_a_model_the_engine_has_no_entry_for_names_the_ones_it_stands(self, tmp_path):
        (tmp_path / "so101.json5").write_text("{}")
        with pytest.raises(ValueError, match="unknown model 'openarm_v2': this engine stands so101"):
            Models.read(tmp_path).of("openarm_v2")

    def test_an_engine_entry_for_a_model_nothing_describes_is_refused(self, tmp_path):
        (tmp_path / "aloha.json5").write_text("{}")
        with pytest.raises(ValueError, match="no entry describes a 'aloha'"):
            Models.read(tmp_path)

    def test_an_engine_with_no_entry_is_refused(self, tmp_path):
        with pytest.raises(RuntimeError, match="holds no model entry"):
            Models.read(tmp_path)

    def test_an_engine_entry_is_an_object(self, tmp_path):
        (tmp_path / "so101.json5").write_text("[]")
        with pytest.raises(RuntimeError, match="must be an object"):
            Models.read(tmp_path)
