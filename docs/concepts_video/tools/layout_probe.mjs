/* Reads the place of each part of one composition at some times.

   Usage: node layout_probe.mjs <chrome> <page url> <composition id> <times>

   `times` holds the times in seconds, with a comma between two times. The
   page is the composition alone, with its styles and its scripts. For each
   time, the script puts the timeline of the composition at the time, and
   prints the boxes and the texts that the page shows, as JSON. A box is an
   element with a border or a fill. A part that changes between the time and
   the next frame is in motion, and the script does not print it. */
import puppeteer from "puppeteer-core";

const [chrome, url, compositionId, timesArgument] = process.argv.slice(2);
const times = timesArgument.split(",").map(Number);
const FRAME_SECONDS = 1 / 30;
/* How long the page can take to load the 3D model of the robot. */
const READY_TIMEOUT_MS = 20000;

/* The parts that the stage of the composition shows. This function runs in
   the page. `data-layout="free"` takes an element and what it holds out of
   the result, and `data-layout="over"` marks them as parts that are over
   other parts on purpose. */
function readParts(id) {
  const alphaOf = function (colour) {
    if (colour === "transparent") return 0;
    const values = colour.match(/[\d.]+/g).map(Number);
    return values.length === 4 ? values[3] : 1;
  };
  const nameOf = function (element) {
    if (element.id) return "#" + element.id;
    const classes = Array.from(element.classList).join(".");
    const owner = element.closest("[id]");
    return (owner ? "#" + owner.id + " " : "") + element.tagName.toLowerCase() + (classes ? "." + classes : "");
  };
  const SIDES = ["Left", "Top", "Right", "Bottom"];
  /* The share of the height of a line of text above its capital letters,
     and the share below its base line, for the two fonts of the video. */
  const LETTERS_TOP_SHARE = 0.2;
  const LETTERS_BOTTOM_SHARE = 0.18;
  const parts = [];
  const visit = function (element, path, opacityAround, layoutAround) {
    if (element instanceof SVGElement) return;
    const style = getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden") return;
    const opacity = opacityAround * Number(style.opacity);
    const layout = element.dataset.layout || layoutAround;
    if (opacity < 0.05 || layout === "free") return;
    const rect = element.getBoundingClientRect();
    const borders = SIDES.map((side) => (alphaOf(style["border" + side + "Color"]) > 0.05 ? parseFloat(style["border" + side + "Width"]) : 0));
    const record = (kind, box, border) =>
      parts.push({ name: nameOf(element), kind, left: box.left, top: box.top, right: box.right, bottom: box.bottom, border, path, isOver: layout === "over", opacity });
    // A border on one side only is a line between two parts, and not a box.
    const hasBorder = borders.every((width) => width > 0);
    const hasFill = alphaOf(style.backgroundColor) > 0.05 || style.backgroundImage !== "none";
    const isPicture = element.tagName === "CANVAS" || element.tagName === "IMG";
    if ((hasBorder || hasFill || isPicture) && rect.width > 0 && rect.height > 0) record("box", rect, borders);
    const lines = [];
    element.childNodes.forEach(function (child) {
      if (child.nodeType !== Node.TEXT_NODE || child.textContent.trim() === "") return;
      const range = document.createRange();
      range.selectNodeContents(child);
      lines.push(...range.getClientRects());
    });
    if (lines.length > 0) {
      // The box of a line of text is higher than its letters: the letters
      // start below its top and end above its bottom.
      record(
        "text",
        {
          left: Math.min(...lines.map((line) => line.left)),
          top: Math.min(...lines.map((line) => line.top + line.height * LETTERS_TOP_SHARE)),
          right: Math.max(...lines.map((line) => line.right)),
          bottom: Math.max(...lines.map((line) => line.bottom - line.height * LETTERS_BOTTOM_SHARE)),
        },
        [0, 0, 0, 0],
      );
    }
    Array.from(element.children).forEach((child, index) => visit(child, path.concat(index), opacity, layout));
  };
  // The stage is the full frame, so it is not a part.
  Array.from(document.getElementById(id + "-stage").children).forEach((child, index) => visit(child, [index], 1, ""));
  return parts;
}

/* Put the timeline of the composition at a time. This function runs in the page. */
function seek(id, time) {
  window.__timelines[id].seek(time, false);
  window.dispatchEvent(new CustomEvent("hf-seek", { detail: { time } }));
}

const sameNumbers = (first, second) => ["left", "top", "right", "bottom", "opacity"].every((key) => Math.abs(first[key] - second[key]) < 0.01);

/* Start Chrome with its software renderer, as the render of the video does.
   On a machine that gives Chrome no sandbox, the start fails: the script
   then starts Chrome with no sandbox, as HyperFrames does. The page is a
   file of this project, which this machine serves. */
async function startChrome() {
  const options = {
    executablePath: chrome,
    args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist", "--hide-scrollbars"],
  };
  try {
    return await puppeteer.launch(options);
  } catch (error) {
    if (!/No usable sandbox|Running as root without --no-sandbox|Failed to move to new namespace/.test(String(error.message))) throw error;
    return puppeteer.launch({ ...options, args: [...options.args, "--no-sandbox"] });
  }
}

const browser = await startChrome();
try {
  const page = await browser.newPage();
  await page.setViewport({ width: 1920, height: 1080 });
  await page.goto(url, { waitUntil: "networkidle0" });
  await page.evaluate(async function (timeout) {
    await document.fonts.ready;
    const ready = Object.values((window.__hf && window.__hf.buildReady) || {});
    await Promise.race([Promise.all(ready), new Promise((resolve) => setTimeout(resolve, timeout))]);
  }, READY_TIMEOUT_MS);
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
