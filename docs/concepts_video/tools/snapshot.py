"""Takes pictures of one part of the video at given times: `pixi run snapshot <part id> <times>`.

A part is one segment, or `splash`: the splash screen with the first segment
after it. The command compiles the video of the part with its voice and its
music into `build/parts/<part id>`, lints it, and asks HyperFrames for one PNG
file at each time, in seconds from the start of that video. It prints the time
of each sentence, and HyperFrames prints the folder of the pictures. Use it to
look at a composition while you write it.
"""

import argparse

from tools import footage, hyperframes, paths, pins, project, voice
from tools.voice_durations import read_voice_durations


def main() -> None:
    parser = argparse.ArgumentParser(description="Take pictures of one part of the concepts video.")
    parser.add_argument("part", help="the id of a segment, for example `opening`, or `splash`")
    parser.add_argument("times", help="the times of the pictures in seconds, for example `0,2.5,10`")
    arguments = parser.parse_args()

    transcript, pinned, footage_list = project.read_project()
    part = project.part_of(transcript, arguments.part)
    pins.ensure_robot_model(pinned)
    footage.fetch_absent(footage_list)
    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    record = voice.record_of(voices, read_voice_durations(paths.VOICE_DURATIONS))

    compiled = paths.BUILD / "parts" / arguments.part
    timeline = project.compile_with_media(part, record, voices, footage_list, compiled)
    slot = timeline.segments[0]
    print(f"The segment `{slot.id}` starts at {slot.start} s and is {slot.duration} s long.")
    for sentence in slot.sentences:
        print(f"  {sentence.id:24s} {slot.start + sentence.start:7.2f} to {slot.start + sentence.end:7.2f}")
    hyperframes.run(["snapshot", str(compiled), "--at", arguments.times])


if __name__ == "__main__":
    main()
