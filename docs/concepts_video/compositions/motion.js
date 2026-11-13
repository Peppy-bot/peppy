/* The animation functions that each composition uses.

   Each function adds tweens to the paused timeline of a composition, at a
   time in seconds from the start of its segment. A frame of the video is a
   function of the time of the timeline only: no function reads a clock or a
   random value. */
window.Motion = (function () {
  const ENTER_SECONDS = 0.5;
  const EXIT_SECONDS = 0.35;

  /* Bring a part in: it is not visible before `at`. */
  function show(timeline, target, at, options) {
    const from = Object.assign({ opacity: 0, y: 26 }, options && options.from);
    const duration = (options && options.duration) || ENTER_SECONDS;
    const stagger = (options && options.stagger) || 0;
    timeline.fromTo(target, from, { opacity: 1, x: 0, y: 0, scale: 1, duration, stagger, ease: "power3.out" }, at);
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

  /* Show a card from `start` to `end`. It comes in from the right side. */
  function card(timeline, target, start, end) {
    showBetween(timeline, target, start, end, { from: { opacity: 0, x: 70, y: 0 } });
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

  /* Bring attention to some lines of a code window: the other lines go dim. */
  function focusLines(timeline, windowSelector, lineNumbers, at) {
    const lines = Array.from(document.querySelectorAll(windowSelector + " .code-line"));
    lines.forEach(function (line, index) {
      const isInFocus = lineNumbers.length === 0 || lineNumbers.indexOf(index + 1) !== -1;
      timeline.to(line, { opacity: isInFocus ? 1 : 0.28, duration: 0.3, ease: "power1.inOut" }, at);
    });
  }

  /* Take the content of a segment out at the end of the segment. The stage
     and the chapter stay, so no frame of the video is empty. */
  function endSegment(timeline, content, segmentSeconds) {
    hide(timeline, content, segmentSeconds);
  }

  return { show, hide, showBetween, card, draw, fade, pulse, travel, focusLines, endSegment };
})();
