/* The 3D view of the robot, for the compositions that show it.

   The model is the turntable model of the OpenArm v2 of the Waldo catalogue.
   A view shows it in a canvas, on a turntable, with nothing behind it: the
   canvas shows the robot only, and the page shows through around it. The
   composition gives the angle of the turntable at some times of its segment,
   and the view draws each frame of its segment from the time of the seek
   only. A view can show the robot on a floor: a disc around the turntable,
   which shows the grid of a simulation or is plain.

   The view puts what it shows in the middle of its canvas at each angle: it
   reads the box of the robot on the picture from the vertices of the model,
   and it shows the part of the picture around that box. The robot has one
   size during the segment: the largest size at which it stays in the canvas
   at each angle of the view.

   A page has one stage, which holds the one WebGL context of the page, and
   each view draws with it and copies the picture into its canvas. The
   compile step bundles this module with three.js into `lib/robot_view.js`,
   which gives `RobotView`. */
import * as THREE from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { MeshoptDecoder } from "three/addons/libs/meshopt_decoder.module.js";

const MODEL_URL = "assets/models/openarm_v2.glb";
const DEGREES = Math.PI / 180;
const UP = new THREE.Vector3(0, 1, 0);
/* The camera looks at the robot from its front right, a little from above. */
const TO_CAMERA = new THREE.Vector3(1.0, 0.42, 0.85).normalize();
const FIELD_OF_VIEW = 16;
/* The share of the width or of the height of the canvas that the robot
   takes, in the direction in which the robot fills the canvas. */
const FILL = 0.94;
/* The view reads the size of the robot at angles this far apart, in degrees. */
const ANGLE_STEP = 2;

/* The points of the robot that a composition can name, in meters, in the
   frame of the model: x to the front of the robot, y up, z to its right. */
const ANCHORS = {
  camera: [0.075, 0.812, 0.0],
  left_wrist: [0.3, 0.485, -0.13],
  right_wrist: [0.3, 0.485, 0.13],
  torso: [0.02, 0.66, 0.0],
  left_shoulder: [-0.01, 0.69, -0.202],
  base: [0.0, 0.02, 0.0],
};

/* The light of the studio around the robot. The robot is dark, on a dark
   page, and the studio has no lamp that makes a hard shadow: each lamp is a
   wide soft area of the studio, so each face of the robot gets light from
   some of them. `towards` gives the place of a lamp as seen from the camera:
   how far to the right, up and to the front (to the camera) it is.
   `spread` says how wide the lamp is, and `colour` is its light. */
const LAMPS = [
  // The key lamp: above the camera, on its left.
  { towards: [-0.7, 0.75, 0.6], spread: 0.09, colour: [9.0, 8.7, 8.5] },
  // The fill lamp: on the right of the camera, at the height of the robot.
  { towards: [0.85, 0.15, 0.6], spread: 0.25, colour: [0.41, 0.43, 0.45] },
  // The two rim lamps, behind the robot: they draw its outline on the page.
  { towards: [-0.75, 0.3, -0.7], spread: 0.07, colour: [12.0, 14.4, 16.0] },
  { towards: [0.8, 0.4, -0.6], spread: 0.07, colour: [12.0, 14.4, 16.0] },
  // The lamp above the robot.
  { towards: [0.0, 1.0, 0.1], spread: 0.12, colour: [0.95, 0.97, 1.0] },
];
/* The light that comes from each direction with no lamp: from the floor,
   from the height of the robot, and from above. */
const AMBIENT = { floor: 0.03, middle: 0.08, top: 0.14 };
const STUDIO_WIDTH = 256;
const STUDIO_HEIGHT = 128;
const EXPOSURE = 0.9;

/* The floor: a disc around the axis of the turntable, in meters. */
const FLOOR_RADIUS = 0.32;
/* The points of the rim of the floor that the view keeps in its canvas. */
const FLOOR_RIM_POINTS = 96;
/* The floor in the video: the colour of the grid of a simulation is the
   colour of data, and the colour of a plain floor is the colour of a line.
   The grid has squares of `CELL` meters, and a width is in pixels. */
const FLOOR_LOOK = `
  const vec3 GRID_COLOUR = vec3(0.373, 0.831, 0.941);
  const vec3 PLAIN_COLOUR = vec3(0.58, 0.667, 0.863);
  const float CELL = 0.07;
  const float LINE_WIDTH = 1.6;
  const float RIM_WIDTH = 3.0;
`;

let stageLoad = null;

/* The stage, made one time for all the views of the page. */
function loadStage() {
  if (stageLoad === null) {
    const loader = new GLTFLoader();
    loader.setMeshoptDecoder(MeshoptDecoder);
    stageLoad = loader.loadAsync(MODEL_URL).then((gltf) => makeStage(gltf.scene));
  }
  return stageLoad;
}

/* The renderer, the scene with its light, the model on its turntable, and
   the floor. `points` holds the vertices of the robot as the robot stands at
   the angle 0 of the turntable, `floorPoints` holds points of the rim of
   the floor, and `reach` is the sphere around the robot at each angle: its
   middle is on the axis of the turntable. */
function makeStage(model) {
  const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(1);
  renderer.setClearColor(0x000000, 0);
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = EXPOSURE;

  const scene = new THREE.Scene();
  const turntable = new THREE.Group();
  turntable.add(model);
  scene.add(turntable);
  scene.environment = makeStudio(renderer);
  const points = pointsOf(turntable);
  const height = new THREE.Box3().setFromArray(points);
  const floor = makeFloor(height.min.y);
  scene.add(floor);
  return { renderer, scene, turntable, floor, points, floorPoints: rimOf(floor), reach: reachOf(points, height) };
}

/* The floor below the robot. `grid` says how much of the grid of a
   simulation the floor shows: at 0, the floor is plain. */
function makeFloor(height) {
  const material = new THREE.ShaderMaterial({
    transparent: true,
    depthWrite: false,
    uniforms: { grid: { value: 0 } },
    vertexShader: `
      varying vec2 place;
      void main() {
        place = position.xy;
        gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
      }
    `,
    fragmentShader: `
      uniform float grid;
      varying vec2 place;
      ${FLOOR_LOOK}
      void main() {
        // How far the nearest line of the grid and the rim of the floor are, in pixels.
        vec2 cell = place / CELL;
        vec2 toLine = abs(fract(cell - 0.5) - 0.5) / fwidth(cell);
        float fromCenter = length(place);
        float toRim = (${FLOOR_RADIUS.toFixed(3)} - fromCenter) / fwidth(fromCenter);
        float line = (1.0 - smoothstep(0.0, LINE_WIDTH, min(toLine.x, toLine.y))) * 0.5 * grid;
        float rim = (1.0 - smoothstep(0.0, RIM_WIDTH, toRim)) * mix(0.55, 0.9, grid);
        float fill = mix(0.1, 0.07, grid);
        gl_FragColor = vec4(mix(PLAIN_COLOUR, GRID_COLOUR, grid), max(fill, max(line, rim)));
      }
    `,
  });
  const floor = new THREE.Mesh(new THREE.CircleGeometry(FLOOR_RADIUS, FLOOR_RIM_POINTS), material);
  // The disc is flat on the ground, a little below the foot of the robot.
  floor.rotation.x = -Math.PI / 2;
  floor.position.y = height - 0.002;
  return floor;
}

/* Points of the rim of the floor, as x, y and z of each one, in the frame of the scene. */
function rimOf(floor) {
  const rim = new Float32Array(FLOOR_RIM_POINTS * 3);
  for (let index = 0; index < FLOOR_RIM_POINTS; index += 1) {
    const around = (index / FLOOR_RIM_POINTS) * 2 * Math.PI;
    rim.set([FLOOR_RADIUS * Math.cos(around), floor.position.y, FLOOR_RADIUS * Math.sin(around)], index * 3);
  }
  return rim;
}

/* The vertices of an object, as x, y and z of each one, in the frame of the scene. */
function pointsOf(object) {
  object.updateMatrixWorld(true);
  const meshes = [];
  object.traverse(function (part) {
    if (part.isMesh) meshes.push(part);
  });
  const count = meshes.reduce((sum, mesh) => sum + mesh.geometry.attributes.position.count, 0);
  const points = new Float32Array(count * 3);
  const vertex = new THREE.Vector3();
  let offset = 0;
  meshes.forEach(function (mesh) {
    const position = mesh.geometry.attributes.position;
    for (let index = 0; index < position.count; index += 1) {
      vertex.fromBufferAttribute(position, index).applyMatrix4(mesh.matrixWorld).toArray(points, offset);
      offset += 3;
    }
  });
  return points;
}

/* The smallest sphere that holds the points at each angle of the turntable,
   with its middle on the axis of the turntable, at the middle of `height`,
   the box of the points. */
function reachOf(points, height) {
  const center = new THREE.Vector3(0, (height.min.y + height.max.y) / 2, 0);
  let radius = 0;
  for (let offset = 0; offset < points.length; offset += 3) {
    radius = Math.max(radius, Math.hypot(points[offset], points[offset + 1] - center.y, points[offset + 2]));
  }
  return new THREE.Sphere(center, radius);
}

/* The light of the studio, as the environment of the scene: the picture of
   the lamps and of the ambient light in each direction around the robot. */
function makeStudio(renderer) {
  const right = new THREE.Vector3().crossVectors(UP, TO_CAMERA).normalize();
  const front = new THREE.Vector3().crossVectors(right, UP);
  const lamps = LAMPS.map(function (lamp) {
    const [toRight, up, toFront] = lamp.towards;
    const direction = new THREE.Vector3().addScaledVector(right, toRight).addScaledVector(UP, up).addScaledVector(front, toFront);
    return { direction: direction.normalize(), spread: lamp.spread, colour: lamp.colour };
  });
  const pixels = new Uint16Array(STUDIO_WIDTH * STUDIO_HEIGHT * 4);
  const direction = new THREE.Vector3();
  for (let row = 0; row < STUDIO_HEIGHT; row += 1) {
    // The first row of the picture is the direction straight down.
    const height = ((row + 0.5) / STUDIO_HEIGHT - 0.5) * Math.PI;
    for (let column = 0; column < STUDIO_WIDTH; column += 1) {
      const around = ((column + 0.5) / STUDIO_WIDTH) * 2 * Math.PI - Math.PI;
      direction.set(Math.cos(height) * Math.cos(around), Math.sin(height), Math.cos(height) * Math.sin(around));
      const light = lightFrom(direction, lamps);
      const offset = (row * STUDIO_WIDTH + column) * 4;
      light.forEach(function (value, channel) {
        pixels[offset + channel] = THREE.DataUtils.toHalfFloat(value);
      });
      pixels[offset + 3] = THREE.DataUtils.toHalfFloat(1);
    }
  }
  const picture = new THREE.DataTexture(pixels, STUDIO_WIDTH, STUDIO_HEIGHT, THREE.RGBAFormat, THREE.HalfFloatType);
  picture.mapping = THREE.EquirectangularReflectionMapping;
  picture.colorSpace = THREE.LinearSRGBColorSpace;
  picture.magFilter = THREE.LinearFilter;
  picture.minFilter = THREE.LinearFilter;
  picture.needsUpdate = true;
  const generator = new THREE.PMREMGenerator(renderer);
  const studio = generator.fromEquirectangular(picture).texture;
  generator.dispose();
  picture.dispose();
  return studio;
}

/* The red, green and blue light that comes to the robot from a direction. */
function lightFrom(direction, lamps) {
  const ambient =
    direction.y < 0
      ? AMBIENT.middle + (AMBIENT.floor - AMBIENT.middle) * -direction.y
      : AMBIENT.middle + (AMBIENT.top - AMBIENT.middle) * direction.y;
  const light = [ambient, ambient, ambient];
  lamps.forEach(function (lamp) {
    const share = Math.exp((direction.dot(lamp.direction) - 1) / lamp.spread);
    lamp.colour.forEach(function (value, channel) {
      light[channel] += value * share;
    });
  });
  return light;
}

/* Give the picture of the stage the size of a view. The stage keeps its
   size from one frame to the next of the same view. */
function resizeStage(stage, width, height) {
  const size = stage.renderer.getSize(new THREE.Vector2());
  if (size.x === width && size.y === height) return;
  stage.renderer.setSize(width, height, false);
}

/* The value at `time`, from `[time, value]` pairs in order. The value
   moves smoothly from one pair to the next. */
function valueAt(pairs, time) {
  if (time <= pairs[0][0]) return pairs[0][1];
  for (let index = 1; index < pairs.length; index += 1) {
    const [endTime, endValue] = pairs[index];
    const [startTime, startValue] = pairs[index - 1];
    if (time <= endTime) {
      const share = (time - startTime) / (endTime - startTime);
      const eased = share * share * (3 - 2 * share);
      return startValue + (endValue - startValue) * eased;
    }
  }
  return pairs[pairs.length - 1][1];
}

/* The angles that the turntable goes through, at most `ANGLE_STEP` apart. */
function anglesOf(turns) {
  const angles = [turns[0][1]];
  for (let index = 1; index < turns.length; index += 1) {
    const start = turns[index - 1][1];
    const end = turns[index][1];
    const steps = Math.ceil(Math.abs(end - start) / ANGLE_STEP);
    for (let step = 1; step <= steps; step += 1) angles.push(start + ((end - start) * step) / steps);
  }
  return angles;
}

/* Put the camera where it sees the sphere around the robot in the height
   of its picture. Each view has the camera at the same place, so each view
   shows the robot with the same perspective. */
function placeCamera(camera, reach) {
  const distance = reach.radius / Math.sin((camera.fov * DEGREES) / 2);
  camera.position.copy(reach.center).addScaledVector(TO_CAMERA, distance);
  camera.near = distance - reach.radius * 1.5;
  camera.far = distance + reach.radius * 1.5;
  camera.lookAt(reach.center);
  camera.updateProjectionMatrix();
  camera.updateMatrixWorld();
}

/* The box of the points on a picture: `picture` is the matrix that gives
   the place of a point of the scene on the picture, where each side of the
   picture is at -1 or 1. */
function boxOnPicture(points, picture) {
  const m = picture.elements;
  const box = { left: Infinity, right: -Infinity, bottom: Infinity, top: -Infinity };
  for (let offset = 0; offset < points.length; offset += 3) {
    const x = points[offset];
    const y = points[offset + 1];
    const z = points[offset + 2];
    const depth = m[3] * x + m[7] * y + m[11] * z + m[15];
    const across = (m[0] * x + m[4] * y + m[8] * z + m[12]) / depth;
    const upward = (m[1] * x + m[5] * y + m[9] * z + m[13]) / depth;
    if (across < box.left) box.left = across;
    if (across > box.right) box.right = across;
    if (upward < box.bottom) box.bottom = upward;
    if (upward > box.top) box.top = upward;
  }
  return box;
}

/* The box that holds two boxes on a picture. */
function boxAround(first, second) {
  return {
    left: Math.min(first.left, second.left),
    right: Math.max(first.right, second.right),
    bottom: Math.min(first.bottom, second.bottom),
    top: Math.max(first.top, second.top),
  };
}

/* The place of the top left corner of an element on the page of the video,
   in pixels of the video. */
function cornerOnPage(element) {
  const corner = { x: 0, y: 0 };
  for (let part = element; part !== null; part = part.offsetParent) {
    corner.x += part.offsetLeft;
    corner.y += part.offsetTop;
  }
  return corner;
}

/* Mount a view of the robot in a canvas of a composition.

   `options.canvas` is the canvas element. `options.segmentStart` and
   `options.segmentDuration` are the time of the start of the segment in the
   video and the duration of the segment, and `options.turns` gives the angle
   of the turntable as `[time in the segment, degrees]` pairs. With
   `options.grid`, the view shows the robot on the floor: `options.grid`
   gives how much of the grid of a simulation the floor shows, from 0 to 1,
   as `[time in the segment, share]` pairs. The view draws at a time of its
   segment only: at each other time, the video does not show its
   composition. The result has `ready`, a promise of the mounted view, and
   `pointOnPage(name, angle)`, the place on the page of an anchor of the
   robot at an angle of the turntable, which is valid when `ready` is. */
function mount(options) {
  const canvas = options.canvas;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  canvas.width = width;
  canvas.height = height;
  const picture = canvas.getContext("2d");
  const camera = new THREE.PerspectiveCamera(FIELD_OF_VIEW, width / height, 0.1, 50);
  const hasFloor = options.grid !== undefined;
  let stage = null;
  /* The full picture of the camera, the box of the floor on it, and how
     many times larger than on it the view shows the robot. */
  let fullPicture = null;
  let floorBox = null;
  let zoom = 1;
  /* The time in the segment. A page starts at the time 0 of the video, and
     HyperFrames sends no seek event for a seek to the time that the page
     has. */
  let time = -options.segmentStart;

  /* The box on the full picture of what the view shows at an angle of the
     turntable: the robot, with the floor when the view has one. */
  function shownBox(angle) {
    const turned = new THREE.Matrix4().makeRotationY(angle * DEGREES).premultiply(fullPicture);
    const robotBox = boxOnPicture(stage.points, turned);
    return hasFloor ? boxAround(robotBox, floorBox) : robotBox;
  }

  /* Show the part of the full picture that has the box in its middle. */
  function aim(angle) {
    const box = shownBox(angle);
    const middle = { x: ((box.left + box.right) / 2 + 1) / 2, y: (1 - (box.bottom + box.top) / 2) / 2 };
    const shown = { width: width / zoom, height: height / zoom };
    camera.setViewOffset(width, height, middle.x * width - shown.width / 2, middle.y * height - shown.height / 2, shown.width, shown.height);
  }

  function draw() {
    if (stage === null || time < 0 || time >= options.segmentDuration) return;
    const angle = valueAt(options.turns, time);
    stage.turntable.rotation.y = angle * DEGREES;
    stage.floor.visible = hasFloor;
    if (hasFloor) stage.floor.material.uniforms.grid.value = valueAt(options.grid, time);
    aim(angle);
    resizeStage(stage, width, height);
    stage.renderer.render(stage.scene, camera);
    picture.clearRect(0, 0, width, height);
    picture.drawImage(stage.renderer.domElement, 0, 0);
  }

  function pointOnPage(name, angle) {
    aim(angle);
    const point = new THREE.Vector3(...ANCHORS[name]);
    point.applyAxisAngle(UP, angle * DEGREES).project(camera);
    const corner = cornerOnPage(canvas);
    return {
      x: corner.x + ((point.x + 1) / 2) * width,
      y: corner.y + ((1 - point.y) / 2) * height,
    };
  }

  window.addEventListener("hf-seek", function (event) {
    time = event.detail.time - options.segmentStart;
    draw();
  });
  const ready = loadStage().then(function (loadedStage) {
    stage = loadedStage;
    placeCamera(camera, stage.reach);
    fullPicture = new THREE.Matrix4().multiplyMatrices(camera.projectionMatrix, camera.matrixWorldInverse);
    floorBox = boxOnPicture(stage.floorPoints, fullPicture);
    const sizes = anglesOf(options.turns).map(function (angle) {
      const box = shownBox(angle);
      return Math.max(box.right - box.left, box.top - box.bottom);
    });
    // Each side of the full picture is at -1 or 1, so the picture is 2 wide and 2 high.
    zoom = (FILL * 2) / Math.max(...sizes);
    draw();
    return { pointOnPage };
  });

  window.__hf = window.__hf || {};
  window.__hf.buildReady = window.__hf.buildReady || {};
  window.__hf.buildReady[canvas.id] = ready;
  return { ready, pointOnPage };
}

export { mount };
