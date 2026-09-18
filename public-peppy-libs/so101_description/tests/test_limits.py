from pathlib import Path

import pytest

from so101_description import limits

URDF = str(Path(__file__).parent / "assets" / "mini_so101.urdf")


def test_limits_parse_from_the_urdf():
    parsed = limits.from_urdf(URDF)
    assert parsed.lower == (-3.1,) * 5
    assert parsed.upper == (3.1,) * 5


def test_contains_and_clamp():
    parsed = limits.from_urdf(URDF)
    assert parsed.contains((0.0, 1.0, -3.1, 3.1, 0.5))
    assert not parsed.contains((0.0, 0.0, 0.0, 3.2, 0.0))
    assert parsed.clamp((9.0, -9.0, 0.5, 0.0, 0.0)) == (3.1, -3.1, 0.5, 0.0, 0.0)


def test_wrong_cardinality_is_refused():
    parsed = limits.from_urdf(URDF)
    assert not parsed.contains(())
    assert not parsed.contains((0.0, 0.0))
    with pytest.raises(ValueError, match="one position per joint"):
        parsed.clamp((0.0,))


def test_transmission_joint_stubs_do_not_shadow_the_real_joints():
    # The production URDF nests limitless <joint> stubs inside <transmission>
    # blocks; they must never shadow the real joints' limits.
    with_transmissions = str(
        Path(__file__).parent / "assets" / "mini_so101_transmissions.urdf"
    )
    parsed = limits.from_urdf(with_transmissions)
    assert parsed.lower == (-3.1,) * 5


def test_missing_joint_fails_loudly(tmp_path):
    stub = tmp_path / "bad.urdf"
    stub.write_text("<robot name='x'><link name='base_link'/></robot>")
    with pytest.raises(ValueError, match="no joint named shoulder_pan"):
        limits.from_urdf(str(stub))
