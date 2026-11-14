/* Reads the place of each part of one composition at some times.

   Usage: node layout_probe.mjs <chrome> <page url> <composition id> <times>

   `times` holds the times in seconds, with a comma between two times. For
   each time, the script puts the timeline of the composition at the time,
   and prints the boxes and the texts that the page shows, as JSON. A part
   that changes between the time and the next frame is in motion, and the
   script does not print it. */
import { FRAME_SECONDS, openComposition, readParts, seek } from "./composition_probe.mjs";

const [chrome, url, compositionId, timesArgument] = process.argv.slice(2);
const times = timesArgument.split(",").map(Number);

const sameNumbers = (first, second) => ["left", "top", "right", "bottom", "opacity"].every((key) => Math.abs(first[key] - second[key]) < 0.01);

const { browser, page } = await openComposition(chrome, url);
try {
  const frames = [];
  for (const time of times) {
    await page.evaluate(seek, compositionId, time + FRAME_SECONDS);
    const next = await page.evaluate(readParts, compositionId);
    await page.evaluate(seek, compositionId, time);
    const parts = await page.evaluate(readParts, compositionId);
    const key = (part) => part.kind + " " + part.path.join(".");
    const nextByKey = new Map(next.map((part) => [key(part), part]));
    const atRest = parts.filter((part) => nextByKey.has(key(part)) && sameNumbers(part, nextByKey.get(key(part))));
    frames.push({ time, parts: atRest });
  }
  process.stdout.write(JSON.stringify(frames));
} finally {
  await browser.close();
}
