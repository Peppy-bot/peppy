import math

import pytest

from control_core_py.params import require_non_empty, require_positive, require_rate


def test_require_positive_returns_the_value():
    assert require_positive("x", 0.25) == 0.25


def test_require_positive_coerces_int_typed_parameters_to_float():
    # A whole-number f64 parameter arrives as a Python int; downstream APIs
    # that type-switch on float must never see it.
    result = require_positive("x", 10)
    assert result == 10.0 and isinstance(result, float)


@pytest.mark.parametrize("bad", [0.0, -1.0, math.nan, math.inf, -math.inf])
def test_require_positive_refuses_nonpositive_and_nonfinite(bad):
    with pytest.raises(ValueError, match="x must be positive and finite"):
        require_positive("x", bad)


def test_require_rate_accepts_the_callers_ceiling():
    assert require_rate("rate", 1000, 1000) == 1000


@pytest.mark.parametrize(("bad", "max_hz"), [(0, 1000), (1001, 1000), (2, 1)])
def test_require_rate_refuses_against_the_callers_ceiling(bad, max_hz):
    with pytest.raises(ValueError, match=f"rate must be in 1..={max_hz}"):
        require_rate("rate", bad, max_hz)


def test_require_non_empty():
    assert require_non_empty("s", "a") == "a"
    with pytest.raises(ValueError, match="s must not be empty"):
        require_non_empty("s", "")
