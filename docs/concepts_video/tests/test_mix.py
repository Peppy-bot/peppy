"""The level of the music at each time of the video."""

import json

from tests.factories import segment_table, splash_table, transcript_of
from tools.mix import BED_LEVEL, DUCK_SECONDS, SPLASH_LEVEL, music_levels, volume_automation
from tools.timing import Timeline, lay_out

TWO_SENTENCES = [{"id": "first", "text": "One."}, {"id": "second", "text": "Two."}]
VOICE_SECONDS = {"opening/first": 2.0, "opening/second": 3.0}


def timeline_of(sentences: list[dict] = TWO_SENTENCES, **changes: object) -> Timeline:
    return lay_out(transcript_of([segment_table("opening", sentences)], **changes), VOICE_SECONDS)


def test_the_music_is_at_its_full_level_in_the_splash_screen_and_below_the_voice_after_it() -> None:
    timeline = timeline_of(splash=splash_table(seconds=4.0))

    assert music_levels(timeline) == [
        (0.0, SPLASH_LEVEL),
        (5.0 - DUCK_SECONDS, SPLASH_LEVEL),
        (5.0, BED_LEVEL),
        (10.5, BED_LEVEL),
        (12.5, 0.0),
    ]


def test_the_music_of_a_video_with_no_splash_screen_starts_below_the_voice() -> None:
    assert music_levels(timeline_of()) == [(0.0, BED_LEVEL), (1.0, BED_LEVEL), (6.5, BED_LEVEL), (8.5, 0.0)]


def test_the_music_goes_out_between_the_end_of_the_voice_and_the_end_of_the_video() -> None:
    timeline = timeline_of()

    (voice_end, level_at_voice_end), (video_end, level_at_video_end) = music_levels(timeline)[-2:]

    assert (voice_end, level_at_voice_end) == (timeline.voice_end, BED_LEVEL)
    assert (video_end, level_at_video_end) == (timeline.duration, 0.0)


def test_a_splash_screen_shorter_than_the_time_to_go_below_the_voice_gives_no_point_before_the_start() -> None:
    timeline = timeline_of(splash=splash_table(seconds=0.2))

    assert music_levels(timeline)[:2] == [(0.0, SPLASH_LEVEL), (1.2, BED_LEVEL)]


def test_a_video_that_ends_with_its_voice_has_no_time_for_the_music_to_go_out() -> None:
    sentences = [{"id": "first", "text": "One."}, {"id": "second", "text": "Two.", "pause_after_seconds": 0.0}]

    assert music_levels(timeline_of(sentences))[-1] == (6.5, BED_LEVEL)


def test_the_levels_are_one_volume_lane_of_hyperframes() -> None:
    automation = json.loads(volume_automation([(0.0, 1.0), (2.5, 0.2)]))

    assert automation == {
        "version": 1,
        "lanes": [{"target": "volume", "points": [{"t": 0.0, "v": 1.0}, {"t": 2.5, "v": 0.2}]}],
    }
