/* The 3D view of the robot, for the compositions that show it.

   The model is the turntable model of the OpenArm v2 of the Waldo catalogue.
   A view draws it in a canvas, on a turntable: the composition gives the
   angle of the turntable at some times of its segment, and the view draws
   each frame from the time of the seek only. The compile step bundles this
   module with three.js into `lib/robot_view.js`, which gives `RobotView`. */
import * as THREE from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { MeshoptDecoder } from "three/addons/libs/meshopt_decoder.module.js";
import { RoomEnvironment } from "three/addons/environments/RoomEnvironment.js";

const MODEL_URL = "assets/models/openarm_v2.glb";
const DEGREES = Math.PI / 180;
/* How near the camera comes: at 1, the sphere around the robot touches the
   top and the bottom of the canvas. */
const FILL = 1.0;

/* The points of the robot that a composition can name, in meters, in the
   frame of the model: x to the front of the robot, y up, z to its right. */
const ANCHORS = {
  camera: [0.075, 0.812, 0.0],
  left_wrist: [0.3, 0.485, -0.13],
};

let modelLoad = null;

/* The model, loaded one time for all the views of the page. */
function loadModel() {
  if (modelLoad === null) {
    const loader = new GLTFLoader();
    loader.setMeshoptDecoder(MeshoptDecoder);
    modelLoad = loader.loadAsync(MODEL_URL).then((gltf) => gltf.scene);
  }
  return modelLoad;
}

/* The angle of the turntable at `time`, from `[time, degrees]` pairs in
   order. The angle moves smoothly from one pair to the next. */
function angleAt(turns, time) {
  if (time <= turns[0][0]) return turns[0][1];
  for (let index = 1; index < turns.length; index += 1) {
    const [endTime, endAngle] = turns[index];
    const [startTime, startAngle] = turns[index - 1];
    if (time <= endTime) {
      const share = (time - startTime) / (endTime - startTime);
      const eased = share * share * (3 - 2 * share);
      return startAngle + (endAngle - startAngle) * eased;
    }
  }
  return turns[turns.length - 1][1];
}

function addLights(scene, renderer) {
  const room = new THREE.PMREMGenerator(renderer).fromScene(new RoomEnvironment(), 0.04);
  scene.environment = room.texture;
  scene.environmentIntensity = 0.85;
  const key = new THREE.DirectionalLight(0xfff1e0, 2.4);
  key.position.set(2.5, 3.5, 2.0);
  scene.add(key);
  const rim = new THREE.DirectionalLight(0x9fdcff, 2.2);
  rim.position.set(-3.0, 2.0, -2.5);
  scene.add(rim);
}

/* A camera that looks at the robot from its front right, a little from
   above, and holds the full robot at each angle of the turntable. */
function frameRobot(camera, robot) {
  const sphere = new THREE.Box3().setFromObject(robot).getBoundingSphere(new THREE.Sphere());
  const distance = sphere.radius / FILL / Math.sin((camera.fov * DEGREES) / 2);
  const direction = new THREE.Vector3(1.0, 0.42, 0.85).normalize();
  camera.position.copy(sphere.center).addScaledVector(direction, distance);
  camera.near = distance / 20;
  camera.far = distance * 4;
  camera.lookAt(sphere.center);
  camera.updateProjectionMatrix();
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

   `options.canvas` is the canvas element. `options.segmentStart` is the time
   of the start of the segment in the video, and `options.turns` gives the
   angle of the turntable as `[time in the segment, degrees]` pairs. The
   result has `ready`, a promise of the mounted view, and `pointOnPage(name,
   angle)`, the place on the page of an anchor of the robot at an angle of the
   turntable, which is valid when `ready` is. */
function mount(options) {
  const canvas = options.canvas;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true });
  renderer.setPixelRatio(1);
  renderer.setSize(width, height, false);
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.15;

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(24, width / height, 0.1, 50);
  const turntable = new THREE.Group();
  scene.add(turntable);
  addLights(scene, renderer);

  function draw(time) {
    turntable.rotation.y = angleAt(options.turns, time) * DEGREES;
    renderer.render(scene, camera);
  }

  function pointOnPage(name, angle) {
    const point = new THREE.Vector3(...ANCHORS[name]);
    point.applyAxisAngle(new THREE.Vector3(0, 1, 0), angle * DEGREES).project(camera);
    const corner = cornerOnPage(canvas);
    return {
      x: corner.x + ((point.x + 1) / 2) * width,
      y: corner.y + ((1 - point.y) / 2) * height,
    };
  }

  const ready = loadModel().then(function (model) {
    turntable.add(model.clone());
    frameRobot(camera, turntable);
    window.addEventListener("hf-seek", function (event) {
      draw(event.detail.time - options.segmentStart);
    });
    draw(0);
    return { pointOnPage };
  });

  window.__hf = window.__hf || {};
  window.__hf.buildReady = window.__hf.buildReady || {};
  window.__hf.buildReady[canvas.id] = ready;
  return { ready, pointOnPage };
}

export { mount };
