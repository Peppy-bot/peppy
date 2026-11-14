"""Makes the music of the video: an ambient bed that plays below the voice.

The music is a calculation, with no recording and no model: slow chords of a
soft pad on a bass note, and a few bell notes with an echo in a large room.
Its duration is its only input, and the same duration always gives the same
samples.
"""

from dataclasses import dataclass
from functools import cache
import math
from pathlib import Path

import numpy
import soundfile

SAMPLE_RATE = 48_000

# The music has 60 beats in one minute, and each chord lasts 8 beats.
BEAT_SECONDS = 1.0
CHORD_SECONDS = 8 * BEAT_SECONDS


@dataclass(frozen=True)
class Chord:
    """The notes of one chord, as MIDI numbers."""

    bass: int
    pad: tuple[int, ...]


# The chords, in D major, which play in turn. The first one is the chord of
# rest. Three notes of the pad stay from one chord to the next: A, E and F sharp.
CHORDS = (
    Chord(bass=50, pad=(57, 61, 64, 66)),  # D major with a ninth
    Chord(bass=47, pad=(57, 62, 64, 66)),  # B minor with an eleventh
    Chord(bass=43, pad=(55, 59, 62, 66)),  # G major with a seventh
    Chord(bass=45, pad=(57, 59, 64, 66)),  # A with a second and a sixth
)
# The music ends on the chord of rest, which plays for this number of chords.
CHORDS_OF_REST_AT_THE_END = 2

# The bell notes of the first chord, then the phrases that play in turn, one
# for each chord. A note is a beat of its chord and a MIDI number. Each note is
# in the pentatonic scale of D major, which agrees with each chord. The count
# of the phrases and the count of the chords have no common divisor, so a
# phrase comes back on the same chord only after all the other phrases.
OPENING_PHRASE = ((0.25, 62), (0.75, 69), (1.25, 74), (1.75, 78), (3.0, 81), (5.5, 76))
PHRASES = (
    ((1.0, 71), (3.0, 78), (4.0, 76)),
    ((0.5, 74), (4.5, 81), (5.5, 78)),
    ((2.0, 76), (3.0, 71)),
    ((0.0, 78), (1.0, 81), (2.0, 83), (6.0, 81)),
    ((3.5, 74), (4.5, 76)),
    ((1.5, 69), (2.5, 76), (5.5, 74), (6.5, 78)),
    ((0.5, 69), (1.5, 74), (2.5, 78), (5.0, 76)),
)

# The strength of each harmonic of a note of the pad and of a bass note, from
# the fundamental.
PAD_HARMONICS = (1.0, 0.4, 0.2, 0.1, 0.05)
BASS_HARMONICS = (1.0, 0.3, 0.1)
# A note of the pad is three voices: the cents from the note, the place
# between the left channel (1.0) and the right channel (0.0), and the
# harmonics. The voice in the centre has the harmonics of the note. The two
# voices at the sides are pure tones a few cents below and above the note, so
# the note moves slowly between the left and the right.
PAD_VOICES = (
    (-5.0, 0.8, (0.25,)),
    (0.0, 0.5, PAD_HARMONICS),
    (5.0, 0.2, (0.25,)),
)
# A note of the pad and a bass note come in and go out in this time, so two
# chords cross. A harmonic comes in later and goes out sooner than the
# harmonic below it, so each chord becomes bright and then soft again.
SWELL_SECONDS = 3.5

# The partials of a bell note: the multiple of its frequency, the strength,
# and the time in which the partial falls to 37 % of its strength.
BELL_PARTIALS = ((1, 1.0, 1.3), (2, 0.35, 0.5), (4, 0.12, 0.15))
BELL_SECONDS = 5.0
BELL_ATTACK_SECONDS = 0.005
BELL_RELEASE_SECONDS = 0.05
# A bell note comes back two times, each time later, weaker and from the other
# side: the delay and the strength of each sound of the note.
BELL_ECHOES = ((0.0, 1.0), (0.75, 0.45), (1.5, 0.2))
# The place of the first sound of a bell note, between the left channel (1.0)
# and the right channel (0.0). The next note of the phrase starts at the
# opposite place.
BELL_SIDE = 0.7

# The strength of each part in the mix. The bass is 5 dB below the pad, and
# the start of a bell note is 8 dB below the pad.
PAD_LEVEL = 1.0
BASS_LEVEL = 1.2
BELL_LEVEL = 1.2

# The room of the bells: their sound falls by 60 dB in this time.
ROOM_DECAY_SECONDS = 2.8
ROOM_DELAY_SECONDS = 0.02
ROOM_LEVEL = 0.7
# The room gives back the low sounds more than the high ones: each sample of
# its response is the mean of this number of samples of noise.
ROOM_SOFTNESS_SAMPLES = 9
ROOM_SEED = 2026

# The gain of the mix. With it, the effective level of the music is 20 dB
# below the full scale, and its highest peak is 5 dB below the full scale.
MIX_GAIN = 0.093


def write_music(seconds: float, destination: Path) -> None:
    """Write the music of a video of `seconds` as a WAV file with two channels."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    soundfile.write(str(destination), music(seconds), SAMPLE_RATE, subtype="PCM_16")


def music(seconds: float) -> numpy.ndarray:
    """The samples of the music of a video of `seconds`: one row for each time, one column for each channel."""
    length = round(seconds * SAMPLE_RATE)
    chords = chords_of(math.ceil(seconds / CHORD_SECONDS))
    chords_sound = numpy.zeros((length, 2))
    bells_sound = numpy.zeros((length, 2))
    for index, chord in enumerate(chords):
        start = index * CHORD_SECONDS
        for note in chord.pad:
            _add(chords_sound, _pad_note(note), start, PAD_LEVEL)
        _add(chords_sound, _bass_note(chord.bass), start, BASS_LEVEL)
        for place, (beat, note) in enumerate(phrase_of(index)):
            _add_bell(bells_sound, note, start + beat * BEAT_SECONDS, starts_on_the_left=place % 2 == 0)
    mix = MIX_GAIN * (chords_sound + _with_room(bells_sound))
    return numpy.round(mix * numpy.iinfo(numpy.int16).max).astype(numpy.int16)


def chords_of(count: int) -> list[Chord]:
    """The chord of each place of a music of `count` chords, which ends on the chord of rest."""
    in_turn = [CHORDS[index % len(CHORDS)] for index in range(count)]
    at_rest = min(count, CHORDS_OF_REST_AT_THE_END)
    return in_turn[: count - at_rest] + [CHORDS[0]] * at_rest


def phrase_of(chord_index: int) -> tuple[tuple[float, int], ...]:
    """The bell notes that play on the chord at `chord_index`."""
    if chord_index == 0:
        return OPENING_PHRASE
    return PHRASES[(chord_index - 1) % len(PHRASES)]


def opening_bell_seconds() -> list[float]:
    """The time of each bell note of the first chord, from the start of the music."""
    return [beat * BEAT_SECONDS for beat, _ in OPENING_PHRASE]


def frequency_of(note: int) -> float:
    """The frequency of a MIDI note, in hertz. The note 69 is the A of 440 Hz."""
    return 440.0 * 2 ** ((note - 69) / 12)


@cache
def _pad_note(note: int) -> numpy.ndarray:
    sound = numpy.zeros((round((CHORD_SECONDS + SWELL_SECONDS) * SAMPLE_RATE), 2))
    for cents, left, harmonics in PAD_VOICES:
        voice = _swell_tone(frequency_of(note) * 2 ** (cents / 1200), harmonics)
        sound += numpy.outer(voice, (left, 1 - left))
    return sound


@cache
def _bass_note(note: int) -> numpy.ndarray:
    return numpy.outer(_swell_tone(frequency_of(note), BASS_HARMONICS), (0.5, 0.5))


@cache
def _bell_note(note: int) -> numpy.ndarray:
    """One bell note, as one channel."""
    times = _times(BELL_SECONDS)
    tone = sum(
        strength * numpy.sin(2 * numpy.pi * frequency_of(note) * multiple * times) * numpy.exp(-times / fall_seconds)
        for multiple, strength, fall_seconds in BELL_PARTIALS
    )
    attack = numpy.minimum(times / BELL_ATTACK_SECONDS, 1.0)
    release = numpy.minimum((BELL_SECONDS - times) / BELL_RELEASE_SECONDS, 1.0)
    return tone * attack * release


def _add_bell(track: numpy.ndarray, note: int, start_seconds: float, starts_on_the_left: bool) -> None:
    """Add a bell note and its echoes. Each sound of the note comes from the side opposite to the sound before it."""
    for index, (delay_seconds, strength) in enumerate(BELL_ECHOES):
        is_on_the_left = starts_on_the_left == (index % 2 == 0)
        left = BELL_SIDE if is_on_the_left else 1 - BELL_SIDE
        sound = numpy.outer(_bell_note(note), (left, 1 - left))
        _add(track, sound, start_seconds + delay_seconds, BELL_LEVEL * strength)


def _add(track: numpy.ndarray, sound: numpy.ndarray, start_seconds: float, level: float) -> None:
    """Add `sound` to `track` at a time. The part of the sound after the end of the track is lost."""
    start = round(start_seconds * SAMPLE_RATE)
    kept = sound[: max(0, len(track) - start)]
    track[start : start + len(kept)] += level * kept


def _times(seconds: float) -> numpy.ndarray:
    """The time of each sample of a sound of `seconds`."""
    return numpy.arange(round(seconds * SAMPLE_RATE)) / SAMPLE_RATE


def _swell_tone(frequency: float, harmonics: tuple[float, ...]) -> numpy.ndarray:
    """A tone that lasts one chord, and comes in and goes out smoothly."""
    times = _times(CHORD_SECONDS + SWELL_SECONDS)
    swell = _swell(len(times))
    return sum(
        strength * numpy.sin(2 * numpy.pi * frequency * number * times) * swell**number
        for number, strength in enumerate(harmonics, start=1)
    )


def _swell(length: int) -> numpy.ndarray:
    """The level of a sound that comes in smoothly at its start and goes out smoothly at its end."""
    edge = round(SWELL_SECONDS * SAMPLE_RATE)
    rise = numpy.sin(numpy.linspace(0, numpy.pi / 2, edge, endpoint=False)) ** 2
    level = numpy.ones(length)
    level[:edge] = rise
    level[length - edge :] = rise[::-1]
    return level


def _with_room(track: numpy.ndarray) -> numpy.ndarray:
    """`track` with the sound that a large room gives back."""
    response = _room_response()
    size = 1 << (len(track) + len(response) - 1).bit_length()
    given_back = numpy.fft.irfft(
        numpy.fft.rfft(track, size, axis=0) * numpy.fft.rfft(response, size, axis=0), size, axis=0
    )
    return track + ROOM_LEVEL * given_back[: len(track)]


def _room_response() -> numpy.ndarray:
    """What the room gives back for one short sound: noise that falls, different in each channel."""
    times = _times(ROOM_DECAY_SECONDS)
    noise = numpy.random.default_rng(ROOM_SEED).standard_normal((len(times), 2))
    softness = numpy.full(ROOM_SOFTNESS_SAMPLES, 1 / ROOM_SOFTNESS_SAMPLES)
    soft_noise = numpy.stack([numpy.convolve(noise[:, channel], softness, mode="same") for channel in (0, 1)], axis=1)
    falling = soft_noise * (10 ** (-3 * times / ROOM_DECAY_SECONDS))[:, None]
    response = numpy.concatenate([numpy.zeros((round(ROOM_DELAY_SECONDS * SAMPLE_RATE), 2)), falling])
    return response / numpy.sqrt(numpy.sum(response**2, axis=0))
