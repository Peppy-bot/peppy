"""Tests for functions.cli module."""

from __future__ import annotations

import os
import ssl
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import (
    PROD_ROUTER_ENDPOINT_ENV,
    RELEASE_TRIPLES,
    ReleaseError,
    _probe_publicly_trusted_tls,
    get_native_triple,
    get_targets_for_platform,
    is_linux,
    is_macos_arm64,
    need_cmd,
    prompt,
    prompt_yn,
    run_with_error_handling,
    validate_release_environment,
    verify_prod_router_publicly_trusted,
)


def test_need_cmd_exists() -> None:
    path = need_cmd("git")
    assert path
    assert "git" in path


def test_need_cmd_missing() -> None:
    with pytest.raises(
        ReleaseError, match="missing required command: nonexistent_cmd_xyz"
    ):
        need_cmd("nonexistent_cmd_xyz")


def test_prompt_with_user_input() -> None:
    with patch("functions.cli.Prompt.ask", return_value="my_answer"):
        result = prompt("Enter value")
    assert result == "my_answer"


def test_prompt_none_input_returns_empty() -> None:
    with patch("functions.cli.Prompt.ask", return_value=None):
        result = prompt("Enter value")
    assert result == ""


def test_prompt_yn_default_yes() -> None:
    with patch("functions.cli.Confirm.ask", return_value=True) as mock_ask:
        prompt_yn("Continue?", default_yes=True)
        mock_ask.assert_called_once_with("Continue?", default=True)


def test_prompt_yn_default_no() -> None:
    with patch("functions.cli.Confirm.ask", return_value=False) as mock_ask:
        prompt_yn("Continue?", default_yes=False)
        mock_ask.assert_called_once_with("Continue?", default=False)


# The prod-router gate runs first on the prod path; patch it out for the tests
# that exercise the token/command checks (the gate has its own tests below).
def test_validate_release_environment_missing_token() -> None:
    with patch.dict(os.environ, {}, clear=True), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ):
        with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
            validate_release_environment(required_commands=())


def test_validate_release_environment_valid_token() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ):
        token = validate_release_environment(required_commands=())
    assert token == "test-token"


def test_validate_release_environment_no_token_required_skips_prod_gate() -> None:
    # require_token=False (the --local path) must skip BOTH the token check and the
    # publicly-trusted prod-router gate. (--base-images skips structurally: it never
    # calls validate_release_environment at all.)
    with patch.dict(os.environ, {}, clear=True), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ) as gate:
        token = validate_release_environment(required_commands=(), require_token=False)
    assert token == ""
    gate.assert_not_called()


def test_validate_release_environment_runs_prod_gate_on_prod_path() -> None:
    # The prod path (require_token=True) invokes the gate before anything else.
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ) as gate:
        validate_release_environment(required_commands=())
    gate.assert_called_once_with()


def test_validate_release_environment_missing_command() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "test-token"}), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ):
        with pytest.raises(ReleaseError, match="missing required command"):
            validate_release_environment(required_commands=("nonexistent_cmd_xyz",))


def test_validate_release_environment_whitespace_token_is_empty() -> None:
    with patch.dict(os.environ, {"GITHUB_PEPPY_RELEASE_TOKEN": "  "}), patch(
        "functions.cli.verify_prod_router_publicly_trusted"
    ):
        with pytest.raises(ReleaseError, match="GITHUB_PEPPY_RELEASE_TOKEN"):
            validate_release_environment(required_commands=())


# --- Publicly-trusted prod-router release gate ---


def test_prod_gate_unset_endpoint_raises() -> None:
    with patch.dict(os.environ, {}, clear=True):
        with pytest.raises(ReleaseError, match=PROD_ROUTER_ENDPOINT_ENV):
            verify_prod_router_publicly_trusted()


def test_prod_gate_malformed_endpoint_raises() -> None:
    with patch.dict(os.environ, {PROD_ROUTER_ENDPOINT_ENV: "no-port-here"}):
        with pytest.raises(ReleaseError, match="host:port"):
            verify_prod_router_publicly_trusted()


def test_prod_gate_valid_chain_passes() -> None:
    # A valid, publicly-trusted endpoint: the probe completes without raising.
    with patch.dict(
        os.environ, {PROD_ROUTER_ENDPOINT_ENV: "health.zenoh.example:7443"}
    ), patch("functions.cli._probe_publicly_trusted_tls") as probe:
        verify_prod_router_publicly_trusted()
    probe.assert_called_once_with("health.zenoh.example", 7443)


def test_prod_gate_untrusted_cert_raises() -> None:
    # A self-signed / private-CA / expired / wrong-name cert surfaces as an
    # SSLCertVerificationError ⇒ "NOT publicly trusted" ReleaseError.
    fake_ctx = MagicMock()
    fake_ctx.wrap_socket.side_effect = ssl.SSLCertVerificationError("self-signed certificate")
    conn_cm = MagicMock()
    conn_cm.__enter__.return_value = MagicMock()  # the connected socket
    conn_cm.__exit__.return_value = False  # do not suppress the raised error
    with patch("functions.cli.ssl.create_default_context", return_value=fake_ctx), patch(
        "functions.cli.socket.create_connection", return_value=conn_cm
    ):
        with pytest.raises(ReleaseError, match="NOT publicly trusted"):
            _probe_publicly_trusted_tls("health.zenoh.example", 7443)


def test_prod_gate_unreachable_raises() -> None:
    # An unreachable / refused endpoint surfaces as OSError ⇒ "could not verify".
    with patch(
        "functions.cli.socket.create_connection", side_effect=OSError("connection refused")
    ):
        with pytest.raises(ReleaseError, match="could not verify"):
            _probe_publicly_trusted_tls("health.zenoh.example", 7443)


def test_run_with_error_handling_catches_release_error() -> None:
    def failing() -> None:
        raise ReleaseError("something went wrong")

    with pytest.raises(SystemExit) as exc_info:
        run_with_error_handling(failing)
    assert exc_info.value.code == 1


def test_run_with_error_handling_catches_keyboard_interrupt() -> None:
    def interrupted() -> None:
        raise KeyboardInterrupt

    with pytest.raises(SystemExit) as exc_info:
        run_with_error_handling(interrupted)
    assert exc_info.value.code == 130


# --- Platform detection tests ---


def test_is_macos_arm64_true() -> None:
    with patch("functions.cli.platform.system", return_value="Darwin"), \
         patch("functions.cli.platform.machine", return_value="arm64"):
        assert is_macos_arm64() is True


def test_is_macos_arm64_false_on_linux() -> None:
    with patch("functions.cli.platform.system", return_value="Linux"), \
         patch("functions.cli.platform.machine", return_value="x86_64"):
        assert is_macos_arm64() is False


def test_is_linux_true() -> None:
    with patch("functions.cli.platform.system", return_value="Linux"):
        assert is_linux() is True


def test_is_linux_false_on_macos() -> None:
    with patch("functions.cli.platform.system", return_value="Darwin"):
        assert is_linux() is False


def test_get_native_triple_macos_arm64() -> None:
    with patch("functions.cli.platform.system", return_value="Darwin"), \
         patch("functions.cli.platform.machine", return_value="arm64"):
        assert get_native_triple() == "aarch64-apple-darwin"


def test_get_native_triple_linux_x86_64() -> None:
    with patch("functions.cli.platform.system", return_value="Linux"), \
         patch("functions.cli.platform.machine", return_value="x86_64"):
        assert get_native_triple() == "x86_64-unknown-linux-gnu"


def test_get_native_triple_linux_aarch64() -> None:
    with patch("functions.cli.platform.system", return_value="Linux"), \
         patch("functions.cli.platform.machine", return_value="aarch64"):
        assert get_native_triple() == "aarch64-unknown-linux-gnu"


def test_get_native_triple_unsupported_platform() -> None:
    with patch("functions.cli.platform.system", return_value="Windows"), \
         patch("functions.cli.platform.machine", return_value="AMD64"):
        with pytest.raises(ReleaseError, match="unsupported platform"):
            get_native_triple()


def test_get_targets_for_platform_macos_returns_all() -> None:
    with patch("functions.cli.platform.system", return_value="Darwin"), \
         patch("functions.cli.platform.machine", return_value="arm64"):
        targets = get_targets_for_platform()
    assert targets == list(RELEASE_TRIPLES)
    assert len(targets) == 3


def test_get_targets_for_platform_linux_returns_native() -> None:
    with patch("functions.cli.platform.system", return_value="Linux"), \
         patch("functions.cli.platform.machine", return_value="x86_64"):
        targets = get_targets_for_platform()
    assert targets == ["x86_64-unknown-linux-gnu"]


def test_get_targets_for_platform_unsupported() -> None:
    with patch("functions.cli.platform.system", return_value="Windows"), \
         patch("functions.cli.platform.machine", return_value="AMD64"):
        with pytest.raises(ReleaseError, match="unsupported platform"):
            get_targets_for_platform()
