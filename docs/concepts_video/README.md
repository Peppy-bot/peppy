# The concepts video

This directory is the project of the video that explains the concepts of Peppy. The quickstart page of the docs shows the video. [HyperFrames](https://github.com/heygen-com/hyperframes) renders it: each composition is an HTML page with CSS and an animation timeline, and a headless Chrome renders it frame by frame to an MP4 file.

The directory is a project of its own. The build of the docs site does not read it.

## Build the video

The build needs [pixi](https://pixi.sh) and Linux on x86_64. When `footage.json` lists footage, it also needs the `WALDO_R2_*` variables of the section [The footage](#the-footage).

```sh
pixi run build
```

The command does the full build with no step by hand:

1. It installs the pinned tools.
2. It gets the footage that the machine does not have, and refuses a file whose hash does not agree with `footage.json`.
3. It makes the voice of each sentence of the transcript.
4. It compiles the compositions, lints them and renders the video.

It writes these files into `build/`:

| File | Use |
|---|---|
| `concepts_video.mp4` | The video. Upload it to YouTube |
| `concepts_video.en.srt` | The captions. Upload them with the video |
| `concepts_video.chapters.txt` | One chapter for each segment. Paste it into the description of the video on YouTube, which makes the chapters from it |

Two builds of the same commit on the same machine give the same MP4 file, byte for byte. The build renders with the software renderer of Chrome and with a fixed count of render workers, so the graphics card and the count of processors of a machine do not change the settings of the render.

YouTube gives each upload a new link. After an upload, put the id of the new link in the `YouTubeEmbed` of `docs/src/content/docs/quickstart.mdx`.

`pixi run build <segment id>` builds the video of one segment only, into `build/<segment id>.mp4`.

## The files

| Path | Content |
|---|---|
| `transcript.toml` | The transcript: the only source of the voice, of the captions and of each text that the video shows |
| `voice_durations.json` | The duration of the voice of each sentence. The build writes it |
| `compositions/` | The composition of each segment and the root composition, as Jinja templates, with the animation functions (`motion.js`) and the 3D view of the robot (`robot_view.js`) |
| `styles/` | The look that all the compositions share |
| `assets/` | The fonts and the pictures |
| `footage.json` | The list of the footage: the name, the SHA-256 and the key of each file |
| `pins.toml`, `pixi.toml`, `pixi.lock`, `package.json`, `package-lock.json` | The version of each tool and file that the build gets |
| `tools/` | The commands |
| `tests/` | The tests of the commands, and the transcript test |

## The transcript and its test

`transcript.toml` has one `[[segment]]` for each segment of the video. A segment holds:

- `chapter`: the title of its chapter.
- `[[segment.source]]`: each section of `docs/src/content/docs` that the segment rests on, with the SHA-256 of the text of the section. A section is the text below one heading, up to the next heading of any level, with the text of the snippet files that it shows. The title of a page names the text before its first heading.
- `[[segment.voice]]`: the sentences of the voice, in order. Each one has an `id`, which its composition uses to time its animation.
- `[segment.screen]`: each text that its composition shows.

A text between two backticks is a technical name: a command, a flag, a file name or a name of an API. In a technical name, a word in angle brackets is a placeholder.

`pixi run test` runs the transcript test (`tests/test_project.py`). It fails in these cases:

- A named section does not exist.
- The text of a named section does not agree with its hash.
- A technical name of a segment is not in the sections that the segment names. For a command, each flag and each run of the other words must be in a section. For a code block, each name must be in a section, but for the words of the language and the names that the block gives in `example_names`.
- A sentence of the voice has more than 25 words, which is the limit that ASD-STE100 gives for a descriptive sentence.
- `voice_durations.json` does not agree with the sentences of the transcript.
- A composition shows a text that is not in the transcript, does not show a text of its segment, or names a footage file that is not in `footage.json`.
- The video is longer than 9 minutes.

The test compares text, counts and hashes only. It calls no model.

When a section of the docs changes, the test gives the new hash of the section. Read the segment against the section, correct the transcript if necessary, then set the hash. A person also checks each change of the transcript against the other rules of ASD-STE100.

When a sentence of the voice changes, run `pixi run build`: it makes the new voice and writes `voice_durations.json`. Commit this file with the transcript.

## The compositions

`compositions/<segment id>.html.j2` is the composition of one segment. The compile step turns it into plain HTML for HyperFrames, in `build/`. A composition holds the layout and the animation, and no text of its own:

- Each text comes from the transcript: `{{ screen.<key> }}` for a text of the segment, `{{ labels.<key> }}` for a word of the frame of a card, and `{{ segment.chapter }}`. The filter `rich` puts each technical name of a text in a `<code>` element.
- Each time comes from the voice: `at.<sentence id>.start` and `at.<sentence id>.end`, in seconds from the start of the segment. `duration` is the duration of the segment, and `start` is its start in the video.
- `footage("<name>")` gives the path of a footage file of `footage.json`.

The parts of `_parts.html.j2` give the chapter, the term card, the comparison card, the comparison of steps and the code window. `Motion` (`motion.js`) gives the animation functions. `RobotView` (`robot_view.js`) draws the 3D model of the OpenArm v2 in a canvas. All the views of the video share one WebGL context, and a view draws during its segment only.

The rules of a composition:

- One visual language in all segments: a node is a box with its interfaces, the stack is a graph that grows, a contract is a socket that several nodes fit, and a machine is a frame around a part of the graph. A term card and a comparison card with ROS 2 have two different looks.
- One term card shows at a time, and it stays long enough to read.
- The id of each element starts with the id of the segment, because HyperFrames puts all the compositions on one page.
- A frame is a function of the time of the timeline only: no clock, and no random value without a seed. Place each tween at a time from `at`.
- An element comes in with `Motion.show` or `gsap.fromTo`, so it is not visible before its time.
- The smallest text is 28 px, and each text stays 96 px or more from the left and right sides of the frame and 54 px or more from its top and bottom.

To look at a composition while you write it:

```sh
pixi run snapshot <segment id> <times>
```

The command compiles the segment and writes one PNG file for each time, for example `pixi run snapshot opening 0,12,28`. `pixi run lint` compiles all the compositions and runs the lint of HyperFrames on them.

## The footage

The footage of the robots and of the simulators is a recording, so no command can make it again. The bucket `concepts-video-assets` of Cloudflare R2 holds it, and the repository holds no footage file. The key of each file contains its SHA-256, so the content at a key never changes.

The build and the command below reach the bucket with three variables of the environment: `WALDO_R2_JURISDICTION_ENDPOINT`, `WALDO_R2_ACCESS_KEY_ID` and `WALDO_R2_SECRET_ACCESS_KEY`. The bucket has no public address.

To add a footage file:

```sh
pixi run add-footage <file>
```

The command uploads the file to the bucket and writes its entry in `footage.json`. The name of the file has lower-case letters, digits and `_` only, for example `opening_two_arms.mp4`. Commit `footage.json`. The original recordings of the camera are not in the bucket: the team keeps them in its own archive.

## The pinned tools

The build gets each tool that changes the result at a pinned version, from its source. The repository does not hold the tools.

| Tool | Pinned in |
|---|---|
| Node.js, FFmpeg, Python, and the Python packages that make the voice | `pixi.toml` and `pixi.lock` |
| HyperFrames, GSAP, three.js and esbuild | `package.json` and `package-lock.json` |
| The Chrome that renders | `pins.toml`. HyperFrames gets the Chrome of its version, and the build refuses another one |
| Kokoro-82M, the model of the voice | `pins.toml`, with the SHA-256 of each file |
| The 3D model of the OpenArm v2: the turntable model of the Waldo catalogue | `pins.toml`, with its SHA-256 |

The fonts are in `assets/fonts`, each with its licence. The build keeps each file that it gets in `.cache/`, and reads no other file of the machine. HyperFrames sends no usage data.

HyperFrames finds a program by its name only in a folder outside the project, and the pixi environment is inside the project. Thus `tools/hyperframes.py` gives HyperFrames the path of the Python, of the FFmpeg and of the FFprobe of the pixi environment. Without these paths, HyperFrames runs the FFmpeg of the machine.

A new build gives the same video until a person changes a version. To change one, change it in its file, run `pixi run build`, and look at the result.

## The checks of CI

CI does not build the video. The job `concepts-video` of the workflow `Tests` runs `pixi run test` and `pixi run lint`, with no voice model, no Chrome and no footage.
