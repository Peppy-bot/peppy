"""The rules of the layout of a frame."""

from tools.layout_rules import BOX, CANVAS, TEXT, Part, frame_faults


def box(name: str, left: float, top: float, width: float, height: float, path: tuple[int, ...], **more: object) -> Part:
    return Part(name=name, kind=BOX, left=left, top=top, right=left + width, bottom=top + height, path=path, **more)


def text(name: str, left: float, top: float, width: float, height: float, path: tuple[int, ...]) -> Part:
    return Part(name=name, kind=TEXT, left=left, top=top, right=left + width, bottom=top + height, path=path)


def canvas(name: str, left: float, top: float, width: float, height: float, path: tuple[int, ...]) -> Part:
    return Part(name=name, kind=CANVAS, left=left, top=top, right=left + width, bottom=top + height, path=path)


def test_a_frame_with_each_part_at_its_place_has_no_fault() -> None:
    row = box("the row", 200, 300, 600, 120, (0,), border=(2, 2, 2, 2))
    name = text("the name", 222, 340, 100, 40, (0, 0))
    first = box("option 1", 340, 322, 210, 76, (0, 1))
    second = box("option 2", 568, 322, 210, 76, (0, 2))

    assert frame_faults([row, name, first, second]) == []


def test_a_part_outside_the_margins_of_the_frame_is_a_fault() -> None:
    card = box("the card", 1400, 900, 500, 150, (0,))

    assert frame_faults([card]) == ["the card is 76 px outside the margins of the frame"]


def test_a_box_that_nearly_touches_one_border_of_its_box_is_a_fault() -> None:
    """The second option of a row is 6 px from the right border of the row, and 20 px from its top."""
    row = box("the row", 200, 300, 900, 128, (0,), border=(2, 2, 2, 2))
    name = text("the name", 228, 344, 100, 40, (0, 0))
    first = box("option 1", 452, 322, 300, 84, (0, 1))
    second = box("option 2", 792, 322, 300, 84, (0, 2))

    assert frame_faults([row, name, first, second]) == [
        "option 2 is 6 px from a border of the row",
        "the content of the row is 26 px from its left side and 6 px from its right side",
    ]


def test_a_part_with_the_same_small_space_on_its_four_sides_is_not_a_fault() -> None:
    seat = box("the seat", 300, 300, 224, 90, (0,), border=(3, 3, 3, 3))
    option = box("the option", 307, 307, 210, 76, (1,))

    assert frame_faults([seat, option]) == []


def test_a_text_that_goes_out_of_its_box_is_a_fault() -> None:
    node = box("the node", 400, 400, 300, 96, (0,), border=(3, 3, 3, 3))
    name = text("the name", 480, 430, 240, 36, (0, 0))

    assert frame_faults([node, name]) == ["the name goes 23 px out of the node"]


def test_content_that_is_nearer_to_the_bottom_of_its_box_than_to_its_top_is_a_fault() -> None:
    window = box("the window", 200, 200, 400, 300, (0,))
    first = box("line 1", 230, 260, 340, 100, (0, 0))
    second = box("line 2", 230, 380, 340, 100, (0, 1))

    assert frame_faults([window, first, second]) == [
        "the content of the window is 60 px from its top side and 20 px from its bottom side"
    ]


def test_two_parts_that_overlap_are_a_fault() -> None:
    label = text("the label", 500, 400, 200, 36, (0,))
    node = box("the node", 650, 380, 300, 96, (1,))

    assert frame_faults([label, node]) == ["the label and the node overlap by 50 px by 36 px"]


def test_a_part_that_is_over_other_parts_on_purpose_is_not_a_fault() -> None:
    frame = box("the frame", 200, 200, 500, 300, (0,), border=(3, 3, 3, 3))
    tab = box("the tab", 228, 176, 160, 48, (0, 0), is_over=True)
    tab_name = Part(name="the name of the tab", kind=TEXT, left=244, top=184, right=372, bottom=216, path=(0, 0), is_over=True)
    mark = box("the mark", 680, 180, 44, 44, (1,), is_over=True)

    assert frame_faults([frame, tab, tab_name, mark]) == []


def test_a_part_in_a_part_of_a_box_is_not_an_overlap_with_the_box() -> None:
    card = box("the card", 200, 200, 600, 400, (0,))
    row = box("the row", 230, 230, 540, 340, (0, 0))
    chip = box("the chip", 260, 260, 200, 60, (5,))

    assert frame_faults([card, row, chip]) == []


def test_a_drawing_in_the_middle_of_its_canvas_is_not_a_fault() -> None:
    robot = canvas("the robot", 200, 200, 400, 600, (0,))
    drawing = box("the robot", 212, 260, 376, 480, (0,))

    assert frame_faults([robot, drawing]) == []


def test_a_drawing_that_is_nearer_to_one_side_of_its_canvas_is_a_fault() -> None:
    """The robot is at the left of its canvas, and a little too low in it."""
    robot = canvas("the robot", 200, 200, 400, 600, (0,))
    drawing = box("the robot", 220, 262, 270, 480, (0,))

    assert frame_faults([robot, drawing]) == [
        "the drawing of the robot is 20 px from the left side of its canvas and 110 px from its right side",
        "the drawing of the robot is 62 px from the top side of its canvas and 58 px from its bottom side",
    ]


def test_a_drawing_that_goes_to_a_side_of_its_canvas_is_a_fault() -> None:
    robot = canvas("the robot", 200, 200, 400, 600, (0,))
    drawing = box("the robot", 200, 260, 400, 480, (0,))

    assert frame_faults([robot, drawing]) == ["the drawing of the robot goes to a side of its canvas"]


def test_a_canvas_is_not_a_part_that_the_page_shows() -> None:
    """A canvas with no drawing goes out of the margins and is over a node: the page shows no fault."""
    robot = canvas("the robot", 40, 200, 400, 600, (0,))
    node = box("the node", 300, 380, 300, 96, (1,))

    assert frame_faults([robot, node]) == []


def test_the_drawing_of_a_canvas_is_a_part_that_the_page_shows() -> None:
    robot = canvas("the robot", 200, 200, 400, 600, (0,))
    drawing = box("the robot", 212, 260, 376, 480, (0,))
    node = box("the node", 500, 380, 300, 96, (1,))

    assert frame_faults([robot, drawing, node]) == ["the robot and the node overlap by 88 px by 96 px"]
