"""The music of the video."""

from pathlib import Path

import numpy
import pytest
import soundfile

from tools import music

FULL_SCALE = numpy.iinfo(numpy.int16).max


def test_the_same_duration_gives_the_same_samples() -> None:
    assert numpy.array_equal(music.music(12.0), music.music(12.0))


def test_the_music_has_two_channels_and_the_duration_that_it_gets() -> None:
    samples = music.music(10.5)

    assert samples.shape == (round(10.5 * music.SAMPLE_RATE), 2)
    assert samples.dtype == numpy.int16


def test_the_music_stays_below_the_full_scale_for_each_chord_and_each_phrase() -> None:
    seconds = len(music.CHORDS) * len(music.PHRASES) * music.CHORD_SECONDS

    samples = music.music(seconds)

    assert numpy.abs(samples.astype(numpy.int32)).max() < 0.75 * FULL_SCALE


def test_the_music_starts_from_silence_and_sounds_with_its_first_bell_note() -> None:
    samples = music.music(4.0).astype(numpy.int32)
    first_bell = round(music.opening_bell_seconds()[0] * music.SAMPLE_RATE)

    assert numpy.abs(samples[: first_bell // 2]).max() < 0.001 * FULL_SCALE
    assert numpy.abs(samples[first_bell : 2 * first_bell]).max() > 0.02 * FULL_SCALE


def test_the_chords_play_in_turn_and_the_music_ends_on_the_chord_of_rest() -> None:
    rest, second, third, fourth = music.CHORDS

    assert music.chords_of(7) == [rest, second, third, fourth, rest, rest, rest]
    assert music.chords_of(1) == [rest]
    assert music.chords_of(0) == []


def test_the_first_chord_has_the_opening_phrase_and_the_other_chords_have_the_phrases_in_turn() -> None:
    assert music.phrase_of(0) == music.OPENING_PHRASE
    assert music.phrase_of(1) == music.PHRASES[0]
    assert music.phrase_of(len(music.PHRASES)) == music.PHRASES[-1]
    assert music.phrase_of(len(music.PHRASES) + 1) == music.PHRASES[0]


def test_each_bell_note_is_in_its_chord_and_in_the_scale_of_the_music() -> None:
    d_major_pentatonic = {2, 4, 6, 9, 11}
    chord_beats = music.CHORD_SECONDS / music.BEAT_SECONDS

    for phrase in (music.OPENING_PHRASE, *music.PHRASES):
        assert all(0 <= beat < chord_beats for beat, _ in phrase)
        assert all(note % 12 in d_major_pentatonic for _, note in phrase)


def test_the_opening_bell_notes_have_their_times_from_the_start_of_the_music() -> None:
    assert music.opening_bell_seconds() == [beat * music.BEAT_SECONDS for beat, _ in music.OPENING_PHRASE]
    assert music.opening_bell_seconds() == sorted(music.opening_bell_seconds())


@pytest.mark.parametrize(("note", "hertz"), [(69, 440.0), (57, 220.0), (81, 880.0)])
def test_a_midi_note_has_its_frequency(note: int, hertz: float) -> None:
    assert music.frequency_of(note) == pytest.approx(hertz)


def test_the_music_file_is_a_wav_file_of_the_duration(tmp_path: Path) -> None:
    file = tmp_path / "assets" / "music.wav"

    music.write_music(2.5, file)

    info = soundfile.info(str(file))
    assert (info.samplerate, info.channels, info.subtype) == (music.SAMPLE_RATE, 2, "PCM_16")
    assert info.frames == round(2.5 * music.SAMPLE_RATE)
