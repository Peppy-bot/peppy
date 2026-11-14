"""The page of one composition, for the checks of the compositions."""

import pytest

from tools import composition_page


def test_the_page_of_a_check_holds_what_the_template_of_the_composition_holds() -> None:
    composition = "<template>\n  <style>#root {}</style>\n  <div id=\"root\"></div>\n</template>\n"

    assert composition_page.composition_content(composition).split() == ["<style>#root", "{}</style>", "<div", 'id="root"></div>']
    with pytest.raises(ValueError, match="has no template element"):
        composition_page.composition_content("<div></div>")
