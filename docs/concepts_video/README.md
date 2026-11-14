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
3. It makes the voice of each sentence of the transcript, and the music.
4. It compiles the compositions, lints them and renders the video.

It writes these files into `build/`:

| File | Use |
|---|---|
| `concepts_video.mp4` | The video. Upload it to YouTube |
| `concepts_video.en.srt` | The captions. Upload them with the video |
| `concepts_video.chapters.txt` | One chapter for each segment. Paste it into the description of the video on YouTube, which makes the chapters from it |

Two builds of the same commit on the same machine give the same MP4 file, byte for byte. The build renders with the software renderer of Chrome and with a fixed count of render workers, so the graphics card and the count of processors of a machine do not change the settings of the render.

YouTube gives each upload a new link. After an upload, put the id of the new link in the `YouTubeEmbed` of `docs/src/content/docs/quickstart.mdx`.

`pixi run build <part id>` builds the video of one part only, into `build/<part id>.mp4`. A part is one segment, or `splash`: the splash screen with the first segment after it. The video of a segment has no splash screen.

## The files

| Path | Content |
|---|---|
| `transcript.toml` | The transcript: the only source of the voice, of the captions and of each text that the video shows |
| `voice_durations.json` | The duration of the voice of each sentence. The build writes it |
| `compositions/` | The composition of each segment, the composition of the splash screen (`splash.html.j2`) and the root composition, as Jinja templates, with the animation functions (`motion.js`) and the 3D view of the robot (`robot_view.js`) |
| `styles/` | The look that all the compositions share |
| `assets/` | The fonts and the pictures |
| `footage.json` | The list of the footage: the name, the SHA-256 and the key of each file |
| `pins.toml`, `pixi.toml`, `pixi.lock`, `package.json`, `package-lock.json` | The version of each tool and file that the build gets |
| `tools/` | The commands |
| `tests/` | The tests of the commands, and the transcript test |

## The transcript and its test

`transcript.toml` has one `[[segment]]` for each segment of the video. A segment holds:

- `chapter`: the title of its chapter.
- `act`: the id of its act, for a segment that is in an act.
- `[[segment.source]]`: each section of `docs/src/content/docs` that the segment rests on, with the SHA-256 of the text of the section. A section is the text below one heading, up to the next heading of any level, with the text of the snippet files that it shows. The title of a page names the text before its first heading.
- `[[segment.voice]]`: the sentences of the voice, in order. Each one has an `id`, which its composition uses to time its animation.
- `[segment.screen]`: each text that its composition shows.

A text between two backticks is a technical name: a command, a flag, a file name or a name of an API. In a technical name, a word in angle brackets is a placeholder.

`[[act]]` is one act: a group of segments that follow each other and answer one question. An act holds its `id`, its `title` and its `question`. The screen shows an act as a part of the video, with its number. The first segment and the last segment are in no act: the first one shows the map of the acts, and the last one shows it again.

`[splash]` is the splash screen, which shows before the first segment with the music and with no voice. `seconds` is its duration, and `[splash.screen]` holds each text that its composition shows. The splash screen names no section of the docs, so its texts hold no technical name. A transcript with no `[splash]` gives a video with no splash screen.

`pixi run test` runs the transcript test (`tests/test_project.py`). It fails in these cases:

- A named section does not exist.
- The text of a named section does not agree with its hash.
- A technical name of a segment is not in the sections that the segment names. For a command, each flag and each run of the other words must be in a section. For a code block, each name must be in a section, but for the words of the language and the names that the block gives in `example_names`.
- A sentence of the voice has more than 25 words, which is the limit that ASD-STE100 gives for a descriptive sentence.
- `voice_durations.json` does not agree with the sentences of the transcript.
- A composition shows a text that is not in the transcript, does not show a text of its segment or of the splash screen, or names a footage file that is not in `footage.json`.
- The video is longer than 10 minutes.

The test compares text, counts and hashes only. It calls no model.

When a section of the docs changes, the test gives the new hash of the section. Read the segment against the section, correct the transcript if necessary, then set the hash. A person also checks each change of the transcript against the other rules of ASD-STE100.

When a sentence of the voice changes, run `pixi run build`: it makes the new voice and writes `voice_durations.json`. Commit this file with the transcript.

## The compositions

`compositions/<segment id>.html.j2` is the composition of one segment. The compile step turns it into plain HTML for HyperFrames, in `build/`. A composition holds the layout and the animation, and no text of its own:

- Each text comes from the transcript: `{{ screen.<key> }}` for a text of the segment, `{{ labels.<key> }}` for a word of the frame of a card, and `{{ segment.chapter }}`. The filter `rich` puts each technical name of a text in a `<code>` element.
- Each time comes from the voice: `at.<sentence id>.start` and `at.<sentence id>.end`, in seconds from the start of the segment. `duration` is the duration of the segment, and `start` is its start in the video.
- The splash screen has no voice. The time of each of its parts comes from the music: `bells` holds the time of each bell note of the start of the music, in seconds.
- `footage("<name>")` gives the path of a footage file of `footage.json`.
- `acts` holds the acts of the video with their chapters, and `act` is the act of the segment.

The parts of `_parts.html.j2` give the header, the map of the acts, the problem scene, the rail, the term card, the comparison of steps, the code window, the node, the command, the icons and the wires. `Motion` (`motion.js`) gives the animation functions.

`RobotView` (`robot_view.js`) draws the 3D model of the OpenArm v2 in a canvas, on a turntable. The canvas shows the robot only, so the page shows through around it. A composition gives the canvas its size, and the view does the rest:

- The robot is in the middle of its canvas at each angle of the turntable. The view reads the box of the robot on the picture from the vertices of the model, and it gives the robot the largest size at which it stays in the canvas at each angle of the view.
- The light is a studio of wide soft lamps (`LAMPS`), with no lamp that makes a hard shadow, so the dark robot is clear on the dark page.
- With `grid`, the robot is on a floor: a disc that shows the grid of a simulation, or is plain.
- `pointOnPage` gives the place on the page of a point of the robot, for a label or a wire.

All the views of the video share one WebGL context, and a view draws during its segment only.

### The structure of the video

The video tells the viewer what it explains before it explains it, and it gives the reason for each concept:

- The first segment says why a robot needs a framework, then shows the map of the video: its three acts, the chapters of each act, and why the viewer needs them.
- The first chapter of an act starts with the map again, with the card of the act in the colour of Peppy.
- Each chapter of an act has the same order. First the problem that the concept solves, in large text. Then the concept, with its diagram. Last, what the viewer gets, with the comparison with ROS 2 when the chapter has one. `Motion.chapter` puts these parts on the timeline of a chapter.
- The last segment shows the map again, with what the viewer gets from each chapter, then the next step.

A chapter keeps its words in the rail, on the right of its diagram, in the same order: the problem, one chip for each term, then one slot that holds the card of the term of the moment and, at the end, what the viewer gets. Thus the viewer sees at each time which problem the diagram answers, and which terms the chapter gave. A segment keeps `problem`, `benefit` and, for the comparison with ROS 2, `ros2` in its `[segment.screen]`.

### The rules of a composition

- One visual language in all segments: a node is a box with its interfaces, the stack is a graph that grows, a contract is a socket that several nodes fit, and a machine is a frame around a part of the graph. A term card and a comparison with ROS 2 have two different looks.
- One layout in all segments: the header at the top, the diagram in the zone on the left (`.zone`), and the rail on the right. A scene with no rail has the full width (`.zone.is-full`).
- One term card shows at a time, and it stays long enough to read.
- The id of each element starts with the id of the segment, because HyperFrames puts all the compositions on one page.
- A frame is a function of the time of the timeline only: no clock, and no random value without a seed. Place each tween at a time from `at`.
- An element comes in with `Motion.show` or `gsap.fromTo`, so it is not visible before its time.
- The smallest text is 28 px, and each part stays 96 px or more from the left and right sides of the frame and 54 px or more from its top and bottom.
- A part that is in a box gets its place from the layout of CSS: a grid or a flex box with a padding. Thus the space on each side of the part comes from one value.
- A wire gets its shape from `Motion.connect`, which reads the place of the two parts that it joins. Thus a wire always ends at its parts.

### The layout check

```sh
pixi run layout [segment id]
```

The command opens each composition, or the one that it names, in the pinned Chrome. At some times of each sentence of the voice, it reads the place of each box and of each text that the page shows. For a canvas, it reads the box of the drawing: the pixels that are not transparent. It fails when it finds one of these faults (`tools/layout_rules.py`):

- A part is outside the margins of the frame.
- A part goes out of the box that it is in, or nearly touches one border of that box.
- The content of a box is nearer to its right side than to its left side, or much nearer to its bottom than to its top.
- Two parts overlap.
- The drawing of a canvas is not in the middle of the canvas, or goes to one of its sides.

In a composition, `data-layout="free"` takes a part out of the check: a decoration, or a message that moves on a wire. `data-layout="over"` says that a part is over other parts on purpose: a tab on a border, a mark on an arrow, a label of a picture. Run the command after each change of a composition, then look at the pictures of the next section: the check reads no wire, and it reads the place of a drawing but not its look.

### The pictures of a composition

To look at a composition while you write it:

```sh
pixi run snapshot <part id> <times>
```

The command compiles the video of one part and writes one PNG file for each time, for example `pixi run snapshot opening 0,12,28` or `pixi run snapshot splash 0.5,2,3.9`.

`pixi run lint` compiles all the compositions and runs the lint of HyperFrames on them. Each error and each warning of the lint is a fault, but for the warning about the number of lines of a compiled composition: a compiled composition is the output of a template, with the texts of the transcript in it.

## The splash screen and the chapters

The video starts with the splash screen. The first segment starts at its end, so each time of the captions and of the chapters includes its duration. YouTube makes chapters only from a list that starts at `0:00`. Thus the first chapter of `concepts_video.chapters.txt` starts at `0:00`, and the splash screen is in it.

## The music

The music is an ambient bed below the voice. It is a calculation (`tools/music.py`), with no recording, no model and no licence: slow chords of a soft pad on a bass note, and a few bell notes with an echo. Its duration is its only input, and the same duration always gives the same samples. The build writes it for the duration of the video, and the repository holds no music file.

The root composition plays the music at a level that changes with the time (`tools/mix.py`):

- While the splash screen shows, the music is at the level of its file, which is near the level of the voice.
- Before the first sentence, the music goes 14 dB down. It stays at this level below the voice.
- After the last sentence, the music goes out until the end of the video.

To change the music, change the chords, the phrases or the levels in `tools/music.py`, run `pixi run build splash`, and listen to `build/splash.mp4`.

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
| Node.js, FFmpeg, Python, and the Python packages that make the voice and the music | `pixi.toml` and `pixi.lock` |
| HyperFrames, GSAP, three.js, esbuild, and Puppeteer for the layout check | `package.json` and `package-lock.json` |
| The Chrome that renders | `pins.toml`. HyperFrames gets the Chrome of its version, and the build refuses another one |
| Kokoro-82M, the model of the voice | `pins.toml`, with the SHA-256 of each file |
| The 3D model of the OpenArm v2: the turntable model of the Waldo catalogue | `pins.toml`, with its SHA-256 |

The fonts are in `assets/fonts`, each with its licence. The build keeps each file that it gets in `.cache/`, and reads no other file of the machine. HyperFrames sends no usage data.

HyperFrames finds a program by its name only in a folder outside the project, and the pixi environment is inside the project. Thus `tools/hyperframes.py` gives HyperFrames the path of the Python, of the FFmpeg and of the FFprobe of the pixi environment. Without these paths, HyperFrames runs the FFmpeg of the machine.

A new build gives the same video until a person changes a version. To change one, change it in its file, run `pixi run build`, and look at the result.

## The checks of CI

CI does not build the video. The job `concepts-video` of the workflow `Tests` runs `pixi run test` and `pixi run lint`, with no voice model, no Chrome and no footage. The tests calculate some music, which needs no file and no model. The layout check needs Chrome and the voice, so a person runs it.
