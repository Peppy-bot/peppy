/* Reads the place of each part of one composition at some times.

   Usage: node layout_probe.mjs <chrome> <page url> <composition id> <times>

   `times` holds the times in seconds, with a comma between two times. The
   page is the composition alone, with its styles and its scripts. For each
   time, the script puts the timeline of the composition at the time, and
   prints the boxes and the texts that the page shows, as JSON. A box is an
   element with a border or a fill, a picture, or the drawing of a canvas:
   the pixels of the canvas that are not transparent. For a canvas, the
   script also prints the canvas itself, which the page does not show. A part
   that changes between the time and the next frame is in motion, and the
   script does not print it. */
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
  /* A pixel of a canvas is a part of its drawing above this alpha, from 0 to 255. */
  const DRAWN_ALPHA = 8;
  /* The box on the page of the drawing of a canvas, or null for a canvas
     with no drawing. The canvases of the video hold a 2D picture. */
  const drawingOf = function (canvas, rect) {
    const context = canvas.getContext("2d");
    if (context === null || canvas.width === 0 || canvas.height === 0) return null;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    const drawn = { left: canvas.width, top: canvas.height, right: -1, bottom: -1 };
    for (let y = 0; y < canvas.height; y += 1) {
      for (let x = 0; x < canvas.width; x += 1) {
        if (pixels[(y * canvas.width + x) * 4 + 3] <= DRAWN_ALPHA) continue;
        drawn.left = Math.min(drawn.left, x);
        drawn.right = Math.max(drawn.right, x);
        drawn.top = Math.min(drawn.top, y);
        drawn.bottom = Math.max(drawn.bottom, y);
      }
    }
    if (drawn.right < 0) return null;
    const scale = { x: rect.width / canvas.width, y: rect.height / canvas.height };
    return {
      left: rect.left + drawn.left * scale.x,
      top: rect.top + drawn.top * scale.y,
      right: rect.left + (drawn.right + 1) * scale.x,
      bottom: rect.top + (drawn.bottom + 1) * scale.y,
    };
  };
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
    const isCanvas = element.tagName === "CANVAS";
    const isShown = rect.width > 0 && rect.height > 0;
    if (isShown && isCanvas) {
      // The page shows the drawing of a canvas, and not the canvas.
      record("canvas", rect, borders);
      const drawing = drawingOf(element, rect);
      if (drawing !== null) record("box", drawing, [0, 0, 0, 0]);
    }
    if (isShown && !isCanvas && (hasBorder || hasFill || element.tagName === "IMG")) record("box", rect, borders);
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
