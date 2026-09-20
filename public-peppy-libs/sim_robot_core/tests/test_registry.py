"""The robots in the scene: who may join, and when a robot's lease runs out."""

from __future__ import annotations

import pytest

from sim_robot_core.models import shipped_entry
from sim_robot_core.registry import Caller, Registry

V1 = shipped_entry("openarm_v1")
V2 = shipped_entry("openarm_v2")


def caller(instance: str = "alpha_init_inst", core_node: str = "sim16") -> Caller:
    return Caller(core_node=core_node, instance_id=instance)


def standing(name: str = "alpha") -> tuple[Registry, Caller]:
    """A registry holding one robot that is in the scene."""
    robots = Registry()
    held = caller()
    robots.admit(name, V2, held, 0.0)
    robots.stand(name, now_s=0.0)
    return robots, held


class TestJoining:
    def test_a_robot_reserves_its_name_and_stands_as_its_model(self):
        robots, held = standing()
        robot = robots.of_name("alpha")
        assert (robot.model, robot.caller) == ("openarm_v2", held)
        assert robot.entry == V2
        assert list(robots.standing()) == ["alpha"]

    def test_a_name_another_robot_stands_under_is_refused(self):
        robots, _ = standing()
        with pytest.raises(ValueError, match="already stands as 'alpha'"):
            robots.admit("alpha", V2, caller("bravo_init_inst"), 0.0)

    def test_a_copy_re_registering_its_own_robot_takes_it_over(self):
        """The same instance attaching the robot it already stands is the
        same live copy registering again: the robot stays exactly as it is."""
        robots, held = standing()
        assert robots.admit("alpha", V2, held, 5.0) is True
        assert robots.of_name("alpha").standing()
        assert list(robots.standing()) == ["alpha"], "one entity, not two"

    def test_a_re_registration_naming_another_model_is_refused(self):
        robots, held = standing()
        with pytest.raises(ValueError, match="stands 'alpha' as openarm_v2"):
            robots.admit("alpha", V1, held, 5.0)
        assert robots.of_name("alpha").model == "openarm_v2"

    def test_a_robot_still_joining_is_not_taken_over(self):
        robots = Registry()
        held = caller()
        robots.admit("alpha", V2, held, 0.0)
        with pytest.raises(ValueError, match="already joining"):
            robots.admit("alpha", V2, held, 1.0)

    def test_a_caller_stands_one_robot(self):
        robots, held = standing()
        with pytest.raises(ValueError, match="already stands a robot"):
            robots.admit("bravo", V2, held, 0.0)

    def test_a_robot_with_no_name_is_refused(self):
        robots = Registry()
        with pytest.raises(ValueError, match="the copy it runs as"):
            robots.admit("", V2, caller(), 0.0)

    def test_a_reserved_robot_is_not_standing_until_the_engine_stands_it(self):
        robots = Registry()
        robots.admit("alpha", V2, caller(), 0.0)
        assert robots.standing() == {}
        assert robots.of_name("alpha").standing() is False

    def test_the_name_of_the_only_robot_is_what_an_uncopied_pair_belongs_to(self):
        robots, _ = standing()
        assert robots.sole_name() == "alpha"
        robots.admit("bravo", V2, caller("bravo_init_inst"), 0.0)
        assert robots.sole_name() is None, "a pair with no copy names no robot in a fleet"

    def test_a_released_name_is_free_again(self):
        robots, _ = standing()
        assert robots.release("alpha").name == "alpha"
        assert robots.of_name("alpha") is None
        robots.admit("alpha", V1, caller("bravo_init_inst"), 0.0)
        assert robots.of_name("alpha").model == "openarm_v1"

    def test_a_robot_is_found_by_the_instance_that_attached_it(self):
        robots, held = standing()
        assert robots.of_caller(held).name == "alpha"
        assert robots.of_caller(caller("nobody")) is None


class TestLeases:
    """The lease stamp each robot holds, which its stay reads to see whether
    the robot still holds the pairs its model asks for."""

    def test_a_robot_holding_its_pairs_marks_its_lease(self):
        robots, _ = standing()
        robots.note_paired({"alpha"}, now_s=10.0)
        assert robots.of_name("alpha").last_paired_s == 10.0

    def test_a_pair_of_a_robot_the_scene_does_not_hold_marks_nothing(self):
        robots, _ = standing()
        robots.note_paired({"nobody"}, now_s=10.0)
        assert robots.of_name("alpha").last_paired_s == 0.0

    def test_a_scene_that_changed_gives_every_robot_its_lease_back(self):
        robots, _ = standing()
        robots.admit("bravo", V2, caller("bravo_init_inst"), 0.0)
        robots.renew(now_s=100.0)
        assert [robot.last_paired_s for robot in robots.robots()] == [100.0, 100.0]
