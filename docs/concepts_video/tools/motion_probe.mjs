/* Reads how each part of one composition shows at each frame of its segment.

   Usage: node motion_probe.mjs <chrome> <page url> <composition id> <frames>

   `frames` is the number of frames of the segment. The script puts the
   timeline of the composition at each frame, and reads the boxes and the
   texts that the page shows. It reads no canvas. It prints one track for
   each part, as JSON: the part, and its states. A state is the number of a
   frame at which the part changes, how much the part shows from 0 to 1, and
   its place: left, top, right, bottom. A part that the page does not show
   has a state of zeros. */
import { FRAME_SECONDS, openComposition, readParts } from "./composition_probe.mjs";

const [chrome, url, compositionId, framesArgument] = process.argv.slice(2);

/* The tracks of the parts of the composition. This function runs in the
   page, which holds `readParts` as `window.__readParts`. */
function readTracks(id, frames, frameSeconds) {
  const HIDDEN = [0, 0, 0, 0, 0];
  const hundredth = (value) => Math.round(value * 100) / 100;
  const tenth = (value) => Math.round(value * 10) / 10;
  const tracks = new Map();
  for (let frame = 0; frame < frames; frame += 1) {
    window.__timelines[id].seek(frame * frameSeconds, false);
    const shown = new Map(window.__readParts(id, { drawings: false }).map((part) => [part.kind + " " + part.path.join("."), part]));
    shown.forEach(function (part, key) {
      if (tracks.has(key)) return;
      tracks.set(key, { name: part.name, kind: part.kind, path: part.path, isOver: part.isOver, border: part.border, words: part.words, states: [] });
    });
    tracks.forEach(function (track, key) {
      const part = shown.get(key);
      const state = part === undefined ? HIDDEN : [hundredth(part.opacity), tenth(part.left), tenth(part.top), tenth(part.right), tenth(part.bottom)];
      const last = track.states[track.states.length - 1];
      if (last === undefined || state.some((value, index) => value !== last[index + 1])) track.states.push([frame, ...state]);
    });
  }
  return Array.from(tracks.values());
}

const { browser, page } = await openComposition(chrome, url);
try {
  await page.evaluate(`window.__readParts = ${readParts}`);
  const tracks = await page.evaluate(readTracks, compositionId, Number(framesArgument), FRAME_SECONDS);
  process.stdout.write(JSON.stringify(tracks));
} finally {
  await browser.close();
}
