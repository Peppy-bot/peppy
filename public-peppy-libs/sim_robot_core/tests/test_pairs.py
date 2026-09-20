"""The four slots, and what each pair names: its robot, its limb or camera."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import pytest

from sim_robot_core.pairs import (
    ARMS,
    GRIPPERS,
    RGB_CAMERAS,
    RGBD_CAMERAS,
    SLOTS,
    Held,
    PairTable,
    camera_of,
    limb_of,
)


@dataclass(frozen=True)
class Producer:
    core_node: str
    instance_id: str


@dataclass(frozen=True)
class Info:
    producer: Producer
    peer_link_id: str


@dataclass(frozen=True)
class Member:
    info: Info
    copy: Optional[str]


def member(instance: str, link: str, copy: Optional[str] = "alpha") -> Member:
    return Member(info=Info(producer=Producer("sim16", instance), peer_link_id=link), copy=copy)


def table(sole: Optional[str] = None, **members) -> PairTable:
    slots = {slot: (lambda held=tuple(members.get(slot, ())): held) for slot in SLOTS}
    return PairTable(slots, lambda: sole)


class TestWhatAPairNames:
    def test_a_limb_is_the_link_the_pair_comes_from_on_the_robots_side(self):
        assert limb_of(member("alpha_backbone_inst", "left_arm")) == "left_arm"

    def test_a_camera_is_its_relays_name_in_the_copy(self):
        assert camera_of(member("alpha_wrist_left", "simulation")) == "wrist_left"

    def test_a_relay_outside_a_copy_runs_under_the_name_the_launcher_wrote(self):
        assert camera_of(member("front", "simulation", copy=None)) == "front"

    def test_a_relay_that_does_not_carry_its_copys_prefix_keeps_its_whole_id(self):
        """Such an id names no camera of any model, so the match refuses it."""
        assert camera_of(member("bravo_front", "simulation", copy="alpha")) == "bravo_front"
        assert camera_of(member("alpha_", "simulation", copy="alpha")) == "alpha_"

    def test_one_misnamed_relay_does_not_stop_the_reading_of_the_other_pairs(self):
        pairs = table(
            arms=[member("alpha_backbone_inst", "left_arm")],
            rgb_cameras=[member("stray", "simulation", copy="alpha")],
        )
        assert pairs.held_by("alpha") == Held(
            arms=frozenset({"left_arm"}), rgb_cameras=frozenset({"stray"})
        )


class TestPairTable:
    def test_one_slot_holds_both_arms_of_one_backbone(self):
        pairs = table(
            arms=[member("alpha_backbone_inst", "left_arm"), member("alpha_backbone_inst", "right_arm")]
        )
        assert pairs.held_by("alpha").arms == frozenset({"left_arm", "right_arm"})

    def test_robots_are_told_apart_by_their_copy(self):
        pairs = table(
            arms=[member("alpha_backbone_inst", "left_arm"), member("charlo_backbone_inst", "arm", "charlo")],
            grippers=[member("charlo_backbone_inst", "gripper", "charlo")],
            rgb_cameras=[member("charlo_front", "simulation", "charlo")],
            rgbd_cameras=[member("alpha_chest", "simulation")],
        )
        assert pairs.held() == {
            "alpha": Held(arms=frozenset({"left_arm"}), rgbd_cameras=frozenset({"chest"})),
            "charlo": Held(
                arms=frozenset({"arm"}),
                grippers=frozenset({"gripper"}),
                rgb_cameras=frozenset({"front"}),
            ),
        }

    def test_a_pair_with_no_copy_belongs_to_the_only_robot_in_the_scene(self):
        pairs = table(sole="backbone_inst", arms=[member("backbone_inst", "arm", copy=None)])
        assert pairs.held_by("backbone_inst").arms == frozenset({"arm"})

    def test_a_pair_with_no_copy_names_no_robot_in_a_fleet(self):
        pairs = table(sole=None, arms=[member("backbone_inst", "arm", copy=None)])
        assert pairs.held() == {}
        assert pairs.pair_of(ARMS, member("backbone_inst", "arm", copy=None).info) is None

    def test_a_setpoint_is_read_for_the_pair_it_came_on(self):
        left = member("alpha_backbone_inst", "left_arm")
        right = member("alpha_backbone_inst", "right_arm")
        pairs = table(arms=[left, right])
        assert pairs.pair_of(ARMS, left.info) == ("alpha", "left_arm")
        assert pairs.pair_of(ARMS, right.info) == ("alpha", "right_arm")

    def test_a_pair_the_slot_let_go_of_names_nothing(self):
        gone = member("alpha_backbone_inst", "left_arm")
        assert table().pair_of(ARMS, gone.info) is None

    def test_a_state_goes_back_on_the_pair_of_its_own_limb(self):
        left = member("alpha_backbone_inst", "left_gripper")
        right = member("alpha_backbone_inst", "right_gripper")
        pairs = table(grippers=[left, right])
        assert pairs.peer_of(GRIPPERS, "alpha", "right_gripper") == right.info
        assert pairs.peer_of(GRIPPERS, "alpha", "gripper") is None
        assert pairs.peer_of(GRIPPERS, "bravo", "right_gripper") is None

    def test_a_frame_goes_to_the_relay_of_its_own_camera(self):
        front = member("charlo_front", "simulation", "charlo")
        pairs = table(rgb_cameras=[front])
        assert pairs.peer_of(RGB_CAMERAS, "charlo", "front") == front.info
        assert pairs.peer_of(RGBD_CAMERAS, "charlo", "front") is None

    def test_a_robot_with_no_pair_holds_nothing(self):
        assert table().held_by("alpha") == Held()
        assert Held().is_empty() and not Held().holds_a_limb()

    def test_a_table_missing_a_slot_is_refused(self):
        with pytest.raises(ValueError, match="lacks \\['rgbd_cameras'\\]"):
            PairTable({slot: tuple for slot in SLOTS if slot != RGBD_CAMERAS}, lambda: None)
