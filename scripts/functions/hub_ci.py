"""The resolve.py of the hub CI action, as the release scripts use it.

.github/actions/hub-ci-peppy/resolve.py holds the one model of the hubs a
release tests and tags: the hub list, the parser of a hub set, the tag a
release puts on each hub, the grammar of a release version, and the checks of
a hub at its commit. The hubs' CI runs it with the runner's python3, so it is
a script of the standard library alone, and this module loads it from its
path as `resolve`.

resolve.py refuses what it cannot use with a ResolveError. Each function of it
that can refuse reaches the release scripts through a function of this module,
which reports the refusal as the ReleaseError a release stage stops on.
"""

from __future__ import annotations

import importlib.util
import sys
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from types import ModuleType

from .cli import ReleaseError

RESOLVE_PATH = (
    Path(__file__).resolve().parents[2]
    / ".github"
    / "actions"
    / "hub-ci-peppy"
    / "resolve.py"
)


def _load_resolve() -> ModuleType:
    spec = importlib.util.spec_from_file_location("hub_ci_resolve", RESOLVE_PATH)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {RESOLVE_PATH}")
    module = importlib.util.module_from_spec(spec)
    # dataclasses look up the module of each class they make in sys.modules.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


resolve = _load_resolve()


@contextmanager
def _refusals_as_release_errors() -> Iterator[None]:
    try:
        yield
    except resolve.ResolveError as e:
        raise ReleaseError(str(e)) from e


def parse_release_version(text: str) -> str:
    """The release version *text* names, v<MAJOR>.<MINOR>.<PATCH>, surrounding
    whitespace dropped."""
    with _refusals_as_release_errors():
        return resolve.parse_release_version(text)


def load_release_set(path: Path) -> resolve.HubSet:
    """The hub set the hub-set job recorded in *path*: every hub, each at the
    commit the release tests and tags."""
    with _refusals_as_release_errors():
        return resolve.read_release_set_file(path)


def require_private_hub_key() -> None:
    """Stop unless an ssh-agent this process reaches holds the deploy key of
    private-nodes-hub, which reads that hub over ssh."""
    with _refusals_as_release_errors():
        resolve.require_private_hub_key(resolve.agent_holds_a_key())


def unpack_peppy(archive: Path, destination: Path) -> Path:
    """Unpack the whole *archive* into *destination* and return its `peppy`
    binary."""
    with _refusals_as_release_errors():
        return resolve.unpack_peppy(archive, destination)


def check_out_commit(resolved: resolve.ResolvedHub, destination: Path) -> None:
    """Check out the hub *resolved* at its commit in *destination*."""
    with _refusals_as_release_errors():
        resolve.check_out_commit(resolved, destination)
