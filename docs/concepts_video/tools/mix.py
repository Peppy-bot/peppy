"""The level of the music at each time of the video.

The music is at its full level while the splash screen shows. It goes below
the voice before the first sentence, stays there, and goes out after the last
sentence.
"""

import json

from tools.timing import Timeline

# The level of the music while the splash screen shows: the level of its file,
# which is near the level of the voice.
SPLASH_LEVEL = 1.0
# The level of the music below the voice: 14 dB below the level of its file.
BED_LEVEL = 0.2
# The music takes this time to go below the voice, and it is there when the
# first sentence starts.
DUCK_SECONDS = 1.5


def music_levels(timeline: Timeline) -> list[tuple[float, float]]:
    """The level of the music as points of a time and a level, with a straight line between two points."""
    start_level = SPLASH_LEVEL if timeline.splash_frames > 0 else BED_LEVEL
    points = [
        (0.0, start_level),
        (round(timeline.voice_start - DUCK_SECONDS, 4), start_level),
        (timeline.voice_start, BED_LEVEL),
        (timeline.voice_end, BED_LEVEL),
        (timeline.duration, 0.0),
    ]
    return _in_order_of_time(points)


def volume_automation(levels: list[tuple[float, float]]) -> str:
    """The levels as the `data-automation` attribute of HyperFrames for an audio element."""
    lane = {"target": "volume", "points": [{"t": time, "v": level} for time, level in levels]}
    return json.dumps({"version": 1, "lanes": [lane]})


def _in_order_of_time(points: list[tuple[float, float]]) -> list[tuple[float, float]]:
    """The points that are after the point before them. A point that is not after the last kept point is lost."""
    kept = points[:1]
    for time, level in points[1:]:
        if time > kept[-1][0]:
            kept.append((time, level))
    return kept
