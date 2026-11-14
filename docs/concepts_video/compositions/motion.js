/* The animation functions that each composition uses.

   Each function adds tweens to the paused timeline of a composition, at a
   time in seconds from the start of its segment. A frame of the video is a
   function of the time of the timeline only: no function reads a clock or a
   random value. */
window.Motion = (function () {
  const ENTER_SECONDS = 0.5;
  const EXIT_SECONDS = 0.35;
  /* The length and the half width of the head of an arrow, in pixels. */
  const HEAD_LENGTH = 16;
  const HEAD_HALF_WIDTH = 10;
  /* The direction that goes out of each side of a box. */
  const OUT_OF = { left: [-1, 0], right: [1, 0], top: [0, -1], bottom: [0, 1] };

  /* Ways for a part to come in, as the `options` of `show`: from a small
     size, from the left side, from the right side, and at its place. */
  const grows = { from: { opacity: 0, scale: 0.6, y: 0 } };
  const fromSide = { from: { opacity: 0, x: -24, y: 0 } };
  const fromRight = { from: { opacity: 0, x: 60, y: 0 } };
  const appears = { from: { opacity: 0, y: 0 } };

  /* A colour of the shared styles, by the name of its variable. */
  function colour(name) {
    return getComputedStyle(document.documentElement).getPropertyValue("--" + name).trim();
  }

  /* The duration of a sentence of the voice, in seconds. */
  function lasts(sentence) {
    return sentence.end - sentence.start;
  }

  /* The time at a share of a sentence of the voice: 0 is its start, 1 its end. */
  function within(sentence, share) {
    return sentence.start + lasts(sentence) * share;
  }

  /* Bring a part in: it is not visible before `at`. */
  function show(timeline, target, at, options) {
    const from = Object.assign({ opacity: 0, y: 26 }, options && options.from);
    const duration = (options && options.duration) || ENTER_SECONDS;
    const stagger = (options && options.stagger) || 0;
    timeline.fromTo(target, from, { opacity: 1, x: 0, y: 0, scale: 1, duration, stagger, ease: "power3.out" }, at);
  }

  /* Bring in the head of an arrow, which does not move. */
  function showHead(timeline, target, at) {
    show(timeline, target, at, { from: { opacity: 0, y: 0 }, duration: 0.2 });
  }

  /* Take a part out: it is not visible at `at`. */
  function hide(timeline, target, at) {
    timeline.to(target, { opacity: 0, duration: EXIT_SECONDS, ease: "power2.in" }, at - EXIT_SECONDS);
  }

  /* Show a part from `start` to `end`. */
  function showBetween(timeline, target, start, end, options) {
    show(timeline, target, start, options);
    hide(timeline, target, end);
  }

  /* Put `to` at the place of `from`: `from` is gone at `at`, and `to` comes
     in from `at`, so the two never show together. */
  function swap(timeline, from, to, at, options) {
    hide(timeline, from, at);
    show(timeline, to, at, options);
  }

  /* Draw an SVG path that has `pathLength="1"`, from its start to its end. */
  function draw(timeline, target, at, duration) {
    timeline.fromTo(
      target,
      { strokeDasharray: 1, strokeDashoffset: 1, opacity: 0 },
      { strokeDashoffset: 0, opacity: 1, duration: duration || 0.7, ease: "power2.inOut" },
      at,
    );
  }

  /* Draw a wire, then bring in its head: the polygon `<wire>-head`. */
  function drawArrow(timeline, wire, at, duration) {
    const seconds = duration || 0.5;
    draw(timeline, wire, at, seconds);
    showHead(timeline, wire + "-head", at + seconds - 0.1);
  }

  /* Change how much a part shows, for example to put it in the background. */
  function fade(timeline, target, opacity, at) {
    timeline.to(target, { opacity, duration: EXIT_SECONDS, ease: "power1.inOut" }, at);
  }

  /* Bring attention to a part with one short pulse. */
  function pulse(timeline, target, at) {
    timeline.fromTo(
      target,
      { scale: 1 },
      { scale: 1.08, duration: 0.22, ease: "power2.out", yoyo: true, repeat: 1, immediateRender: false },
      at,
    );
  }

  /* Bring attention to a wide part with one short glow of its border. */
  function glow(timeline, target, at) {
    timeline.to(target, { borderColor: colour("peppy"), duration: 0.25, ease: "power1.inOut", yoyo: true, repeat: 1 }, at);
  }

  /* Give a part the colours of `look` from `at`, for example a new border. */
  function paint(timeline, target, look, at) {
    timeline.to(target, Object.assign({ duration: 0.3, ease: "power1.inOut" }, look), at);
  }

  /* Move a part in a straight line from one point to another, `times` times.
     The part shows only while it moves. */
  function travel(timeline, target, from, to, at, duration, times) {
    gsap.set(target, { opacity: 0 });
    timeline.fromTo(
      target,
      { x: from.x, y: from.y, opacity: 1 },
      { x: to.x, y: to.y, duration, ease: "none", repeat: (times || 1) - 1, immediateRender: false },
      at,
    );
    timeline.set(target, { opacity: 0 }, at + duration * (times || 1));
  }

  /* Move a part along an SVG path, from its start to its end, `times` times.
     The part goes in straight lines between points of the path. It shows
     only while it moves. */
  function follow(timeline, target, pathSelector, at, duration, times) {
    const STEPS = 12;
    const turns = times || 1;
    const path = document.querySelector(pathSelector);
    const length = path.getTotalLength();
    const points = [];
    for (let step = 0; step <= STEPS; step += 1) {
      const point = path.getPointAtLength((length * step) / STEPS);
      points.push({ x: point.x, y: point.y });
    }
    gsap.set(target, { opacity: 0 });
    for (let turn = 0; turn < turns; turn += 1) {
      for (let step = 0; step < STEPS; step += 1) {
        timeline.fromTo(
          target,
          { x: points[step].x, y: points[step].y, opacity: 1 },
          { x: points[step + 1].x, y: points[step + 1].y, duration: duration / STEPS, ease: "none", immediateRender: false },
          at + turn * duration + (step * duration) / STEPS,
        );
      }
    }
    timeline.set(target, { opacity: 0 }, at + duration * turns);
  }

  /* Bring attention to some lines of a code window: the other lines go dim. */
  function focusLines(timeline, windowSelector, lineNumbers, at) {
    const lines = Array.from(document.querySelectorAll(windowSelector + " .code-line"));
    lines.forEach(function (line, index) {
      const isInFocus = lineNumbers.length === 0 || lineNumbers.indexOf(index + 1) !== -1;
      timeline.to(line, { opacity: isInFocus ? 1 : 0.28, duration: 0.3, ease: "power1.inOut" }, at);
    });
  }

  /* Take the content of a segment out at the end of the segment. The stage
     and the header stay, so no frame of the video is empty. */
  function endSegment(timeline, content, segmentSeconds) {
    hide(timeline, content, segmentSeconds);
  }

  /* The place and the size of a part on the stage, in pixels of the video.
     A transform does not change the result, so the place is the one that the
     part has when it is at rest. `x` and `y` are the middle of the part. */
  function boxOf(target) {
    const element = typeof target === "string" ? document.querySelector(target) : target;
    let left = 0;
    let top = 0;
    for (let part = element; part !== null; part = part.offsetParent) {
      left += part.offsetLeft;
      top += part.offsetTop;
      if (part !== element) {
        left += part.clientLeft;
        top += part.clientTop;
      }
    }
    const width = element.offsetWidth;
    const height = element.offsetHeight;
    return { left, top, width, height, right: left + width, bottom: top + height, x: left + width / 2, y: top + height / 2 };
  }

  /* A point of a side of a part: `side` is `left`, `right`, `top` or
     `bottom`. `shift` moves the point along the side from its middle, in
     pixels to the right or down. A target that is a point of the stage, as
     `{ x, y }`, gives that point. */
  function anchor(target, side, shift) {
    if (typeof target !== "string" && target.nodeType === undefined) return { x: target.x, y: target.y };
    const box = boxOf(target);
    const along = shift || 0;
    if (side === "left") return { x: box.left, y: box.y + along };
    if (side === "right") return { x: box.right, y: box.y + along };
    if (side === "top") return { x: box.x + along, y: box.top };
    return { x: box.x + along, y: box.bottom };
  }

  /* Give an SVG path the line from one part to another, and give the polygon
     `<wire>-head`, when the page has one, the head of the arrow at its end.

     `from` and `to` are `[part, side, shift]`, as the arguments of `anchor`.
     The line goes out of the side of `from` and comes into the side of `to`.
     When the two points are not on one straight line, the line has one round
     corner, or two when the two sides face each other. When the two sides
     are the same side, the line goes `options.reach` pixels out of the
     farther part, then comes back. `options.gap` and `options.endGap` are
     the space between the line and each part, and `options.curve` makes a
     smooth curve of a line with two corners. The result holds the two ends
     of the line. */
  function connect(wire, from, to, options) {
    const settings = Object.assign({ gap: 0, endGap: 6, radius: 24, curve: false, reach: 56 }, options);
    const out = OUT_OF[from[1]];
    const into = OUT_OF[to[1]];
    const first = anchor(from[0], from[1], from[2]);
    const last = anchor(to[0], to[1], to[2]);
    const start = { x: first.x + out[0] * settings.gap, y: first.y + out[1] * settings.gap };
    const tip = { x: last.x + into[0] * settings.endGap, y: last.y + into[1] * settings.endGap };
    const head = document.querySelector(wire + "-head");
    const back = head === null ? 0 : HEAD_LENGTH - 2;
    const end = { x: tip.x + into[0] * back, y: tip.y + into[1] * back };
    document.querySelector(wire).setAttribute("d", lineBetween(start, out, end, into, settings));
    if (head !== null) {
      const base = { x: tip.x + into[0] * HEAD_LENGTH, y: tip.y + into[1] * HEAD_LENGTH };
      const across = [into[1] * HEAD_HALF_WIDTH, into[0] * HEAD_HALF_WIDTH];
      head.setAttribute(
        "points",
        `${base.x - across[0]},${base.y - across[1]} ${tip.x},${tip.y} ${base.x + across[0]},${base.y + across[1]}`,
      );
    }
    return { start, end: tip };
  }

  /* The path from `start`, which goes in the direction `out`, to `end`,
     which the line reaches against the direction `into`. */
  function lineBetween(start, out, end, into, settings) {
    const isLevel = out[0] !== 0;
    const endsLevel = into[0] !== 0;
    const move = `M ${start.x} ${start.y}`;
    if (isLevel && endsLevel) {
      if (Math.abs(start.y - end.y) < 0.5) return `${move} H ${end.x}`;
      const isSameSide = out[0] === into[0];
      const farthest = out[0] > 0 ? Math.max(start.x, end.x) : Math.min(start.x, end.x);
      const middle = isSameSide ? farthest + out[0] * settings.reach : (start.x + end.x) / 2;
      if (isSameSide) {
        const radius = Math.min(settings.radius, Math.abs(end.y - start.y) / 2);
        const down = Math.sign(end.y - start.y);
        return (
          `${move} H ${middle - out[0] * radius} Q ${middle} ${start.y} ${middle} ${start.y + down * radius} ` +
          `V ${end.y - down * radius} Q ${middle} ${end.y} ${middle - out[0] * radius} ${end.y} H ${end.x}`
        );
      }
      if (settings.curve) return `${move} C ${middle} ${start.y}, ${middle} ${end.y}, ${end.x} ${end.y}`;
      const radius = Math.min(settings.radius, Math.abs(end.y - start.y) / 2, Math.abs(end.x - start.x) / 2);
      const down = Math.sign(end.y - start.y);
      const across = Math.sign(end.x - start.x);
      return (
        `${move} H ${middle - across * radius} Q ${middle} ${start.y} ${middle} ${start.y + down * radius} ` +
        `V ${end.y - down * radius} Q ${middle} ${end.y} ${middle + across * radius} ${end.y} H ${end.x}`
      );
    }
    if (!isLevel && !endsLevel) {
      if (Math.abs(start.x - end.x) < 0.5) return `${move} V ${end.y}`;
      const middle = (start.y + end.y) / 2;
      if (settings.curve) return `${move} C ${start.x} ${middle}, ${end.x} ${middle}, ${end.x} ${end.y}`;
      const radius = Math.min(settings.radius, Math.abs(end.x - start.x) / 2, Math.abs(end.y - start.y) / 2);
      const across = Math.sign(end.x - start.x);
      const down = Math.sign(end.y - start.y);
      return (
        `${move} V ${middle - down * radius} Q ${start.x} ${middle} ${start.x + across * radius} ${middle} ` +
        `H ${end.x - across * radius} Q ${end.x} ${middle} ${end.x} ${middle + down * radius} V ${end.y}`
      );
    }
    const radius = Math.min(settings.radius, Math.abs(end.x - start.x), Math.abs(end.y - start.y));
    const across = Math.sign(end.x - start.x);
    const down = Math.sign(end.y - start.y);
    if (isLevel) {
      return `${move} H ${end.x - across * radius} Q ${end.x} ${start.y} ${end.x} ${start.y + down * radius} V ${end.y}`;
    }
    return `${move} V ${end.y - down * radius} Q ${start.x} ${end.y} ${start.x + across * radius} ${end.y} H ${end.x}`;
  }

  /* The point of an SVG path at a share of its length: 0 is its start, 1 its end. */
  function pointOn(pathSelector, share) {
    const path = document.querySelector(pathSelector);
    const point = path.getPointAtLength(path.getTotalLength() * share);
    return { x: point.x, y: point.y };
  }

  /* The terms of a chapter, in the rail. Each entry is `[key, start, end]`.
     The card `<id>-card-<key>` shows from `start` to `end`, and the chip
     `<id>-chip-<key>` comes in at `start` and stays. The chip of the term of
     the moment has the colour of Peppy. */
  function terms(timeline, id, entries) {
    const lit = { borderColor: colour("peppy"), color: colour("ink") };
    const quiet = { borderColor: colour("line-strong"), color: colour("ink-dim") };
    entries.forEach(function ([key, start, end]) {
      showBetween(timeline, `${id}-card-${key}`, start, end, fromRight);
      show(timeline, `${id}-chip-${key}`, start, grows);
      paint(timeline, `${id}-chip-${key}`, lit, start);
      paint(timeline, `${id}-chip-${key}`, quiet, end - 0.3);
    });
  }

  /* The parts that each chapter of an act has, in the order of the chapter:
     the map of the acts when the chapter opens an act, the problem in large
     text, then the concept with the rail, which ends with what the viewer
     gets.

     `plan.map` is `[start, end]` of the scene that opens an act, with the
     map of the video, in a chapter that opens an act.
     `plan.problem` is `[start, end]` of the problem scene. The concept scene
     comes in at its end, when the problem scene is gone, or at `plan.concept`
     in a chapter that has another scene between the two. `plan.terms` holds
     the entries of `terms`.
     `plan.benefit` is the time of what the viewer gets, and `plan.ros2` the
     time of the note about ROS 2, in a chapter that has one. */
  function chapter(timeline, id, plan) {
    timeline.fromTo(`${id}-glow`, { x: -50, y: -30 }, { x: 70, y: 40, duration: plan.duration, ease: "sine.inOut" }, 0);
    endSegment(timeline, `${id}-content`, plan.duration);

    if (plan.map) {
      showBetween(timeline, `${id}-act`, plan.map[0], plan.map[1], appears);
      show(timeline, `${id}-act .act-heading`, plan.map[0], fromSide);
      show(timeline, `${id}-act .map-act`, plan.map[0] + 0.1, { stagger: 0.12 });
      pulse(timeline, `${id}-act .map-act.is-current`, plan.map[0] + 1.1);
    }

    const [problemStart, problemEnd] = plan.problem;
    showBetween(timeline, `${id}-problem`, problemStart, problemEnd, appears);
    show(timeline, `${id}-problem .scene-label`, problemStart, fromSide);
    show(timeline, `${id}-problem .problem-headline`, problemStart + 0.15, fromSide);
    show(timeline, `${id}-problem .problem-number`, problemStart, { from: { opacity: 0, y: 60 }, duration: 0.9 });

    const conceptStart = plan.concept === undefined ? problemEnd : plan.concept;
    show(timeline, `${id}-concept`, conceptStart, appears);
    show(timeline, `${id}-rail-problem`, conceptStart + 0.2, fromRight);
    terms(timeline, id, plan.terms || []);

    if (plan.benefit !== undefined) {
      show(timeline, `${id}-benefit`, plan.benefit, fromRight);
      glow(timeline, `${id}-benefit`, plan.benefit + 0.6);
    }
    if (plan.ros2 !== undefined) {
      show(timeline, `${id}-ros2`, plan.ros2, fromRight);
    }
  }

  return {
    grows,
    fromSide,
    fromRight,
    appears,
    colour,
    lasts,
    within,
    show,
    showHead,
    hide,
    showBetween,
    swap,
    draw,
    drawArrow,
    fade,
    pulse,
    glow,
    paint,
    travel,
    follow,
    focusLines,
    endSegment,
    boxOf,
    connect,
    pointOn,
    chapter,
  };
})();
