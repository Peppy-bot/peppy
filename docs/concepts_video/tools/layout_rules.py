"""The rules of the layout of a frame of the video.

A frame is a list of parts, as the page shows them at one time: each part is
a box (an element with a border or a fill, a picture, or the drawing of a
canvas) or a text, with its place in the frame. The rules find the faults
that a person sees as a part that is not at its place: a part outside the
margins of the frame, a part that crosses or nearly touches the border of the
box that it is in, a box whose content is nearer to one side than to the
opposite side, two parts that overlap, and a drawing that is not in the
middle of its canvas.

A canvas is a part too, but the page does not show it: it shows its drawing
only. Thus a canvas counts only for the rule of its drawing.

The rules read numbers only, so the same frame always gives the same result.
"""

from dataclasses import dataclass

# The margins of the frame: each part stays inside them.
FRAME_LEFT = 96.0
FRAME_TOP = 54.0
FRAME_RIGHT = 1920.0 - 96.0
FRAME_BOTTOM = 1080.0 - 54.0
# A difference that the eye does not see, in pixels.
TOLERANCE = 0.5
# The share of the area of a part that a box holds when the part is in the box.
INSIDE_SHARE = 0.85
# The smallest space between a box and the border of the box that it is in,
# and between a text and the left and right borders of its box.
MIN_BOX_GAP = 8.0
MIN_TEXT_GAP = 6.0
# The four spaces around a part are the same when they differ by less than this.
SAME_GAP = 2.0
# The content of a box fills the box in one direction from this share of its size.
FILLS_FROM_SHARE = 0.7
# How much nearer to the right side than to the left side the content can be.
SIDE_TOLERANCE = 6.0
# The space below the content is too small below this share of the space above it.
MIN_BOTTOM_SHARE = 0.5
# A drawing is in the middle of its canvas when the spaces on two opposite
# sides of it differ by less than this.
MIDDLE_TOLERANCE = 3.0
# A drawing nearer than this to a side of its canvas goes out of the canvas.
MIN_DRAWING_GAP = 1.0

BOX = "box"
TEXT = "text"
# A canvas element. Its drawing is a box with the same path.
CANVAS = "canvas"
# A part that is over other parts on purpose: a tab on a border, a mark on an
# arrow, a label of a picture. It can cross a border and overlap a part.
OVER = "over"


@dataclass(frozen=True)
class Part:
    name: str
    kind: str
    left: float
    top: float
    right: float
    bottom: float
    # The place of the element of the part in the page, as the index of each
    # ancestor in its parent, from the stage.
    path: tuple[int, ...]
    is_over: bool = False
    # The width of the border of a box on each side: left, top, right, bottom.
    border: tuple[float, float, float, float] = (0.0, 0.0, 0.0, 0.0)

    @property
    def width(self) -> float:
        return self.right - self.left

    @property
    def height(self) -> float:
        return self.bottom - self.top

    @property
    def area(self) -> float:
        return max(self.width, 0.0) * max(self.height, 0.0)

    def is_in_the_element_of(self, other: "Part") -> bool:
        """True when the element of this part is the element of `other`, or is inside it."""
        return self.path[: len(other.path)] == other.path


def frame_faults(parts: list[Part]) -> list[str]:
    """The faults of one frame, as sentences for the person who corrects the composition."""
    shown = [part for part in parts if part.kind != CANVAS]
    containers = _containers(shown)
    return (
        _outside_the_frame(shown)
        + _at_the_border_of_their_box(shown, containers)
        + _nearer_to_one_side(shown, containers)
        + _overlaps(shown)
        + _drawings_off_the_middle(parts)
    )


def _outside_the_frame(parts: list[Part]) -> list[str]:
    faults: list[str] = []
    for part in parts:
        over = max(
            FRAME_LEFT - part.left, part.right - FRAME_RIGHT, FRAME_TOP - part.top, part.bottom - FRAME_BOTTOM
        )
        if over > TOLERANCE:
            faults.append(f"{part.name} is {over:.0f} px outside the margins of the frame")
    return faults


def _containers(parts: list[Part]) -> dict[int, int]:
    """For the index of each part, the index of the box that the part is in: the smallest box that holds it."""
    containers: dict[int, int] = {}
    for index, part in enumerate(parts):
        if part.is_over or part.area == 0.0:
            continue
        holders = [other for other, box in enumerate(parts) if box.kind == BOX and _holds(box, part)]
        if holders:
            containers[index] = min(holders, key=lambda other: parts[other].area)
    return containers


def _holds(box: Part, part: Part) -> bool:
    """True when `part` is in `box`: the box is larger and holds most of the area of the part."""
    if box is part or box.area <= part.area:
        return False
    is_inside_the_part = box.is_in_the_element_of(part) and box.path != part.path
    if is_inside_the_part:
        return False
    return _shared_area(box, part) >= INSIDE_SHARE * part.area


def _shared_area(first: Part, second: Part) -> float:
    width = min(first.right, second.right) - max(first.left, second.left)
    height = min(first.bottom, second.bottom) - max(first.top, second.top)
    return max(width, 0.0) * max(height, 0.0)


def _gaps(part: Part, box: Part) -> tuple[float, float, float, float]:
    """The space between `part` and the inner side of each border of `box`: left, top, right, bottom."""
    return (
        part.left - box.left - box.border[0],
        part.top - box.top - box.border[1],
        box.right - box.border[2] - part.right,
        box.bottom - box.border[3] - part.bottom,
    )


def _at_the_border_of_their_box(parts: list[Part], containers: dict[int, int]) -> list[str]:
    """The parts that cross the border of their box, or that nearly touch it on one side."""
    faults: list[str] = []
    for index, box_index in containers.items():
        part, box = parts[index], parts[box_index]
        left, top, right, bottom = _gaps(part, box)
        if part.kind == TEXT:
            # The top and the bottom of a text are those of its letters, which
            # the font places in the line. Thus only its left and right sides
            # say how near to a border the text is.
            smallest = min(left, right)
            if smallest < MIN_TEXT_GAP:
                faults.append(_border_fault(part, box, smallest))
            continue
        gaps = (left, top, right, bottom)
        if min(gaps) < MIN_BOX_GAP and max(gaps) - min(gaps) > SAME_GAP:
            faults.append(_border_fault(part, box, min(gaps)))
    return faults


def _border_fault(part: Part, box: Part, gap: float) -> str:
    if gap < -TOLERANCE:
        return f"{part.name} goes {-gap:.0f} px out of {box.name}"
    return f"{part.name} is {max(gap, 0.0):.0f} px from a border of {box.name}"


def _nearer_to_one_side(parts: list[Part], containers: dict[int, int]) -> list[str]:
    """The boxes whose content is nearer to the right side than to the left, or too near the bottom.

    Content that goes out of its box is a fault of `_at_the_border_of_their_box`, and not of this rule.
    """
    content: dict[int, list[Part]] = {}
    for index, box_index in containers.items():
        content.setdefault(box_index, []).append(parts[index])
    faults: list[str] = []
    for box_index, inside in content.items():
        box = parts[box_index]
        left, top, right, bottom = (min(gaps) for gaps in zip(*(_gaps(part, box) for part in inside)))
        fills_the_width = box.width - left - right >= FILLS_FROM_SHARE * box.width
        if right >= 0.0 and fills_the_width and right + SIDE_TOLERANCE < left:
            faults.append(
                f"the content of {box.name} is {left:.0f} px from its left side and {right:.0f} px from its right side"
            )
        fills_the_height = box.height - top - bottom >= FILLS_FROM_SHARE * box.height
        if bottom >= 0.0 and fills_the_height and bottom < MIN_BOTTOM_SHARE * top:
            faults.append(
                f"the content of {box.name} is {top:.0f} px from its top side and {bottom:.0f} px from its bottom side"
            )
    return faults


def _overlaps(parts: list[Part]) -> list[str]:
    """The pairs of parts that overlap, when no part of the pair is in the other."""
    faults: list[str] = []
    for index, first in enumerate(parts):
        for second in parts[index + 1 :]:
            if first.is_over or second.is_over:
                continue
            if first.is_in_the_element_of(second) or second.is_in_the_element_of(first):
                continue
            if _shared_area(first, second) >= INSIDE_SHARE * min(first.area, second.area):
                continue
            width = min(first.right, second.right) - max(first.left, second.left)
            height = min(first.bottom, second.bottom) - max(first.top, second.top)
            if width > 1.0 and height > 1.0:
                faults.append(f"{first.name} and {second.name} overlap by {width:.0f} px by {height:.0f} px")
    return faults


def _drawings_off_the_middle(parts: list[Part]) -> list[str]:
    """The canvases whose drawing goes to one of their sides, or is not in their middle."""
    drawings = {part.path: part for part in parts if part.kind == BOX}
    faults: list[str] = []
    for canvas in parts:
        if canvas.kind != CANVAS or canvas.path not in drawings:
            continue
        left, top, right, bottom = _gaps(drawings[canvas.path], canvas)
        if min(left, top, right, bottom) < MIN_DRAWING_GAP:
            faults.append(f"the drawing of {canvas.name} goes to a side of its canvas")
            continue
        if abs(left - right) > MIDDLE_TOLERANCE:
            faults.append(
                f"the drawing of {canvas.name} is {left:.0f} px from the left side of its canvas"
                f" and {right:.0f} px from its right side"
            )
        if abs(top - bottom) > MIDDLE_TOLERANCE:
            faults.append(
                f"the drawing of {canvas.name} is {top:.0f} px from the top side of its canvas"
                f" and {bottom:.0f} px from its bottom side"
            )
    return faults
