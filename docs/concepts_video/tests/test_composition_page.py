"""The page of one composition, for the checks of the compositions."""

from pathlib import Path
import subprocess

import pytest

from tools import composition_page, hyperframes
from tools.timing import SegmentSlot


def test_the_page_of_a_check_holds_what_the_template_of_the_composition_holds() -> None:
    composition = "<template>\n  <style>#root {}</style>\n  <div id=\"root\"></div>\n</template>\n"

    assert composition_page.composition_content(composition).split() == ["<style>#root", "{}</style>", "<div", 'id="root"></div>']
    with pytest.raises(ValueError, match="has no template element"):
        composition_page.composition_content("<div></div>")


def test_a_probe_runs_without_display_or_vulkan_driver_variables(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    compositions = tmp_path / "compositions"
    compositions.mkdir()
    (compositions / "opening.html").write_text("<template><div>ok</div></template>", encoding="utf-8")
    for name in hyperframes.HEADLESS_OMITTED_VARIABLES:
        monkeypatch.setenv(name, "set-by-host")

    seen_env: dict[str, str] = {}

    def fake_run(command: list[str], *, env: dict[str, str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        seen_env.update(env)
        return subprocess.CompletedProcess(command, 0, stdout='{"ok": true}', stderr="")

    monkeypatch.setattr(composition_page.subprocess, "run", fake_run)
    segment = composition_page.CompiledSegment(
        id="opening",
        folder=tmp_path,
        slot=SegmentSlot(id="opening", start_frame=0, frames=30, sentences=()),
        chrome=tmp_path / "chrome",
    )

    assert segment.probe(tmp_path / "probe.mjs", "0") == {"ok": True}
    for name in hyperframes.HEADLESS_OMITTED_VARIABLES:
        assert name not in seen_env
