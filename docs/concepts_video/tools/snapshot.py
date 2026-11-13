"""Takes pictures of one segment at given times: `pixi run snapshot <segment id> <times>`.

The command compiles the composition of one segment with its voice into
`build/segments/<segment id>`, lints it, and asks HyperFrames for one PNG file
at each time, in seconds from the start of the segment. It prints the time of each sentence, and HyperFrames prints
the folder of the pictures. Use it to look at a composition while you write it.
"""

import argparse
from dataclasses import replace

from tools import compiler, hyperframes, paths, pins, project, voice
from tools.voice_durations import read_voice_durations


def main() -> None:
    parser = argparse.ArgumentParser(description="Take pictures of one segment of the concepts video.")
    parser.add_argument("segment", help="the id of the segment, for example `opening`")
    parser.add_argument("times", help="the times of the pictures in seconds, for example `0,2.5,10`")
    arguments = parser.parse_args()

    transcript, pinned, footage_list = project.read_project()
    voices = voice.make_sentence_voices(transcript, pins.voice_tools_identity(pinned))
    record = voice.record_of(voices, read_voice_durations(paths.VOICE_DURATIONS))
    one_segment = replace(transcript, segments=(transcript.segment(arguments.segment),))
    timeline = project.timeline_of(one_segment, record)

    compiled = paths.BUILD / "segments" / arguments.segment
    compiler.compile_project(one_segment, timeline, footage_list, compiled)
    compiler.link_robot_model(pins.ensure_robot_model(pinned), compiled)
    voice.assemble_voice_track(one_segment, timeline, voices, compiled / "assets" / "voice.wav")
    slot = timeline.segments[0]
    print(f"The segment `{slot.id}` is {slot.duration} s long.")
    for sentence in slot.sentences:
        print(f"  {sentence.id:24s} {sentence.start:7.2f} to {sentence.end:7.2f}")
    hyperframes.lint(compiled, media_are_present=True)
    hyperframes.run(["snapshot", str(compiled), "--at", arguments.times])


if __name__ == "__main__":
    main()
