//! Render the dual-arm hull model (and optionally the source meshes) at a
//! configuration into a self-contained interactive HTML scene.
//!
//! ```sh
//! cargo run --release --example visualize -- \
//!     --urdf tests/fixtures/openarm_v10.urdf --meshes tests/fixtures/meshes \
//!     --left-base openarm_left_link0 --right-base openarm_right_link0 \
//!     --left 0,0,1.2,0.4,0,0,0 --right 0,0,-1.2,0.4,0,0,0 \
//!     --gripper 0.0 --wireframes -o scene.html
//! ```
//!
//! `--gripper <f>` (or per side `--left-gripper` / `--right-gripper`) sets the
//! gripper opening as a fraction in `[0, 1]` (0 = closed, 1 = fully open, the
//! default). The finger hulls, and their `--wireframes` source meshes, are placed
//! at that opening, so the scene shows the true finger positions rather than the
//! full swept envelope.
//!
//! A circumscribing fit carries no rounding, so a piece draws as its bare
//! polytope faces, which is exactly the surface the distance query measures
//! against. A piece that does carry a rounding radius additionally gets cylinders
//! along its edges and spheres at its vertices, standing in for the fillets of
//! the hull's Minkowski sum with a ball; those are tessellated, so that surface is
//! drawn to within the tessellation while GJK measures the true one.
//! `--wireframes` overlays the source
//! meshes underneath, so the gap between mesh and piece surface is the fit's
//! slack, visible directly. The closest pair is highlighted with the GJK/EPA
//! witness segment; the HUD shows the signed minimum distance against the band
//! thresholds.
use std::collections::HashSet;

use bimanual_collision_model::nalgebra::{Point3, Vector3};
use bimanual_collision_model::urdf_collision::UrdfCollisions;
use bimanual_collision_model::{BimanualCollisionModel, PairSpec, PlacedPiece};
use srs_model::{ARM_DOF, Arm, JointVec};

#[path = "../tests/fixtures/openarm.rs"]
mod openarm;

/// Wireframes are decimated to about this many triangles per body.
const MAX_WIRE_TRIS: usize = 2000;

fn main() {
    if let Err(e) = run() {
        eprintln!("visualize: {e}");
        std::process::exit(1);
    }
}

struct Args {
    urdf: String,
    meshes: String,
    left_base: String,
    right_base: String,
    left: JointVec,
    right: JointVec,
    /// Gripper opening per side as a fraction in `[0, 1]` (0 = closed, 1 = open).
    left_gripper: f64,
    right_gripper: f64,
    out: String,
    wireframes: bool,
    d_stop: f64,
    d_safe: f64,
    exclude: Vec<PairSpec>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        urdf: String::new(),
        meshes: String::new(),
        left_base: String::new(),
        right_base: String::new(),
        left: [0.0; ARM_DOF],
        right: [0.0; ARM_DOF],
        left_gripper: 1.0,
        right_gripper: 1.0,
        out: "scene.html".into(),
        wireframes: false,
        d_stop: 0.01,
        d_safe: 0.03,
        exclude: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        let fraction = |s: &str| -> Result<f64, String> {
            let f: f64 = s
                .parse()
                .map_err(|e| format!("bad gripper fraction '{s}': {e}"))?;
            (0.0..=1.0)
                .contains(&f)
                .then_some(f)
                .ok_or(format!("gripper fraction must be in [0, 1], got {f}"))
        };
        match flag.as_str() {
            "--urdf" => args.urdf = value()?,
            "--meshes" | "-m" => args.meshes = value()?,
            "--left-base" => args.left_base = value()?,
            "--right-base" => args.right_base = value()?,
            "--left" | "-l" => args.left = parse_joints(&value()?)?,
            "--right" | "-r" => args.right = parse_joints(&value()?)?,
            "--gripper" | "-g" => {
                let f = fraction(&value()?)?;
                args.left_gripper = f;
                args.right_gripper = f;
            }
            "--left-gripper" => args.left_gripper = fraction(&value()?)?,
            "--right-gripper" => args.right_gripper = fraction(&value()?)?,
            "--out" | "-o" => args.out = value()?,
            "--wireframes" | "-w" => args.wireframes = true,
            "--d-stop" => args.d_stop = value()?.parse().map_err(|e| format!("{e}"))?,
            "--d-safe" => args.d_safe = value()?.parse().map_err(|e| format!("{e}"))?,
            "--exclude" | "-x" => {
                let v = value()?;
                let (a, b) = v
                    .split_once(',')
                    .ok_or(format!("--exclude wants 'link_a,link_b', got '{v}'"))?;
                args.exclude.push(PairSpec::new(a.trim(), b.trim()));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if args.urdf.is_empty()
        || args.meshes.is_empty()
        || args.left_base.is_empty()
        || args.right_base.is_empty()
    {
        return Err(
            "required: --urdf <file> --meshes <dir> --left-base <link> --right-base <link>".into(),
        );
    }
    Ok(args)
}

fn parse_joints(s: &str) -> Result<JointVec, String> {
    let vals: Vec<f64> = s
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|e| format!("bad joint '{p}': {e}"))
        })
        .collect::<Result<_, _>>()?;
    vals.try_into()
        .map_err(|v: Vec<f64>| format!("expected {ARM_DOF} joints, got {}", v.len()))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    // The model is a pure distance oracle; the HUD colours by the caller's own
    // d_stop/d_safe (the --d-stop / --d-safe args), independent of any band.
    // Supply the tight torso proxy we ship whenever this is the OpenArm robot,
    // so the scene shows the real fit; any other URDF falls through to auto-fit.
    let mut builder = BimanualCollisionModel::builder_from_file(
        &args.urdf,
        &args.meshes,
        &args.left_base,
        &args.right_base,
    )?
    .exclude(&args.exclude);
    if UrdfCollisions::from_file(&args.urdf)?
        .collision_link_names()
        .iter()
        .any(|n| n == openarm::TORSO_BODY)
    {
        builder = builder.regions(openarm::TORSO_BODY, openarm::torso_regions());
    }
    let mut model = builder.build()?;
    // Place the fingers at the requested opening; every query below then sees the
    // fingers where they actually are, not their full swept envelope.
    model.set_gripper_openings(args.left_gripper, args.right_gripper);

    let proximity = model.min_distance(&args.left, &args.right)?;
    let (distance, witness) = (
        proximity.distance,
        [point_json(&proximity.on_a), point_json(&proximity.on_b)],
    );
    let (link_a, link_b) = (proximity.link_a.to_string(), proximity.link_b.to_string());

    let bodies: Vec<serde_json::Value> = model
        .world_pieces(&args.left, &args.right)?
        .into_iter()
        .flat_map(|(name, pieces)| {
            let side = if name.contains("_left_") {
                "left"
            } else if name.contains("_right_") {
                "right"
            } else {
                "fixed"
            };
            let hit = name == link_a || name == link_b;
            pieces
                .into_iter()
                .map(move |p| rounded_piece_json(&p, side, hit))
                .collect::<Vec<_>>()
        })
        .collect();

    let meshes = if args.wireframes {
        mesh_wireframes(&args)?
    } else {
        Vec::new()
    };

    let data = serde_json::json!({
        "dStop": args.d_stop,
        "dSafe": args.d_safe,
        "distance": round(distance),
        "pair": format!("{link_a}  vs  {link_b}"),
        "witness": witness,
        "bodies": bodies,
        "meshes": meshes,
    });
    // Escape "</script>" / "<!--" in any name so it cannot end the script early.
    let html = TEMPLATE.replace("__DATA__", &data.to_string().replace('<', "\\u003c"));
    std::fs::write(&args.out, html).map_err(|e| format!("write {}: {e}", args.out))?;
    println!(
        "wrote {} (d={distance:+.4} m, {link_a} vs {link_b})",
        args.out
    );
    Ok(())
}

/// Decimated source-mesh triangles per body in world frame, from the same URDF
/// and STL pipeline the fit used. Fingers are drawn at the requested opening
/// ([`Args::left_gripper`] / [`Args::right_gripper`]), matching the live finger
/// hulls rather than a swept envelope.
fn mesh_wireframes(args: &Args) -> Result<Vec<serde_json::Value>, String> {
    let urdf = UrdfCollisions::from_file(&args.urdf)?;
    let chain_links: Vec<String> = [&args.left_base, &args.right_base]
        .iter()
        .flat_map(|base| {
            let arm = Arm::from_urdf_file(&args.urdf, base).expect("arm");
            let posed = arm.at(&[0.0; ARM_DOF]);
            (0..ARM_DOF)
                .map(|i| posed.link_name(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let attached: Vec<String> = chain_links
        .iter()
        .flat_map(|l| urdf.children_of(l))
        .filter(|c| !chain_links.contains(c))
        .collect();

    let mut out = Vec::new();
    for name in urdf.collision_link_names() {
        if chain_links.contains(&name) || attached.contains(&name) {
            continue;
        }
        out.push(wire_json(decimate(
            &urdf.fixed_vertices_in_root(&name, &args.meshes)?,
        )));
    }
    for (base, q, opening) in [
        (&args.left_base, &args.left, args.left_gripper),
        (&args.right_base, &args.right, args.right_gripper),
    ] {
        let arm = Arm::from_urdf_file(&args.urdf, base).map_err(|e| e.to_string())?;
        let posed = arm.at(q);
        for i in 0..ARM_DOF {
            let name = posed.link_name(i);
            let pose = posed.link_pose_world(i);
            let mut verts: Vec<Point3<f64>> = urdf
                .link_vertices(name, &args.meshes)?
                .iter()
                .map(|v| pose * v)
                .collect();
            // Fixed children ride with the link; movable children are the
            // gripper fingers, drawn at the requested opening.
            let mut fingers: Vec<(String, f64, f64)> = Vec::new();
            for child in urdf.children_of(name) {
                if chain_links.contains(&child) || urdf.collisions_of(&child).is_empty() {
                    continue;
                }
                let joint = urdf
                    .parent_joint(&child)
                    .expect("children_of implies a parent joint");
                if joint.is_fixed() {
                    verts.extend(
                        urdf.child_vertices_in_parent(&child, joint.lower_limit, &args.meshes)?
                            .iter()
                            .map(|v| pose * v),
                    );
                } else {
                    fingers.push((child, joint.lower_limit, joint.upper_limit));
                }
            }
            // Orient the finger travel the way the model does (the extreme with
            // the larger between-finger separation is open; a mirrored gripper
            // flips the URDF limit order), so the wireframe fingers land on the
            // same pose as the placed hulls. The model build already rejected
            // any link whose movable children are not a two-finger pair.
            if let [fa, fb] = fingers.as_mut_slice() {
                let centroid = |child: &str, q: f64| -> Result<Point3<f64>, String> {
                    let vs = urdf.child_vertices_in_parent(child, q, &args.meshes)?;
                    let sum = vs.iter().fold(Vector3::zeros(), |a, p| a + p.coords);
                    Ok(Point3::from(sum / vs.len().max(1) as f64))
                };
                let sep_lower = (centroid(&fa.0, fa.1)? - centroid(&fb.0, fb.1)?).norm();
                let sep_upper = (centroid(&fa.0, fa.2)? - centroid(&fb.0, fb.2)?).norm();
                if sep_lower > sep_upper {
                    std::mem::swap(&mut fa.1, &mut fa.2);
                    std::mem::swap(&mut fb.1, &mut fb.2);
                }
            }
            for (child, closed, open) in &fingers {
                let qc = closed + opening * (open - closed);
                verts.extend(
                    urdf.child_vertices_in_parent(child, qc, &args.meshes)?
                        .iter()
                        .map(|v| pose * v),
                );
            }
            out.push(wire_json(decimate(&verts)));
        }
    }
    Ok(out)
}

/// Keep every k-th triangle so each body stays under [`MAX_WIRE_TRIS`].
fn decimate(verts: &[Point3<f64>]) -> Vec<Point3<f64>> {
    let step = (verts.len() / 3).div_ceil(MAX_WIRE_TRIS).max(1);
    verts
        .chunks_exact(3)
        .step_by(step)
        .flatten()
        .copied()
        .collect()
}

/// One hull piece as render data: the faces offset outward by any rounding
/// radius (the flat caps), the bare vertices (sphere centres), and the unique
/// edges (cylinder axes). At `radius == 0` the caps alone are the surface the
/// distance query sees, drawn exactly and with no fillets. Otherwise the browser
/// sweeps a ball of `radius` over the edges and vertices, so caps + edge
/// cylinders + vertex spheres union into that surface up to the tessellation of
/// the spheres and cylinders.
fn rounded_piece_json(p: &PlacedPiece, side: &str, hit: bool) -> serde_json::Value {
    let centroid = Point3::from(
        p.vertices
            .iter()
            .fold(Vector3::zeros(), |a, v| a + v.coords)
            / p.vertices.len() as f64,
    );
    let mut caps: Vec<f64> = Vec::new();
    for f in &p.faces {
        let (a, b, c) = (p.vertices[f[0]], p.vertices[f[1]], p.vertices[f[2]]);
        let mut n = (b - a).cross(&(c - a));
        // Orient outward by the centroid; the hull's own winding is not relied on.
        let face_center = Point3::from((a.coords + b.coords + c.coords) / 3.0);
        if n.dot(&(face_center - centroid)) < 0.0 {
            n = -n;
        }
        let offset = if n.norm_squared() > 1e-20 {
            n.normalize() * p.radius
        } else {
            Vector3::zeros()
        };
        for v in [a, b, c] {
            caps.extend([
                round(v.x + offset.x),
                round(v.y + offset.y),
                round(v.z + offset.z),
            ]);
        }
    }
    let verts = flat(&p.vertices);
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut edges: Vec<f64> = Vec::new();
    for f in &p.faces {
        for (i, j) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])] {
            let edge = if i < j { (i, j) } else { (j, i) };
            if seen.insert(edge) {
                edges.extend(flat(&[p.vertices[edge.0], p.vertices[edge.1]]));
            }
        }
    }
    serde_json::json!({ "side": side, "hit": hit, "radius": round(p.radius), "caps": caps, "verts": verts, "edges": edges })
}

fn flat(verts: &[Point3<f64>]) -> Vec<f64> {
    verts
        .iter()
        .flat_map(|p| [round(p.x), round(p.y), round(p.z)])
        .collect()
}

fn wire_json(verts: Vec<Point3<f64>>) -> serde_json::Value {
    serde_json::json!({ "positions": flat(&verts) })
}

fn point_json(p: &Point3<f64>) -> serde_json::Value {
    serde_json::json!([round(p.x), round(p.y), round(p.z)])
}

fn round(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

const TEMPLATE: &str = r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"/><title>bimanual_collision_model scene</title>
<style>
  body { margin:0; background:#15171c; color:#d7dae0; font:13px/1.4 system-ui, sans-serif; }
  #hud { position:fixed; top:10px; left:10px; background:rgba(21,23,28,.85); padding:10px 12px; border:1px solid #333; border-radius:6px; }
  #hud b.bad{color:#ff5a5a} #hud b.warn{color:#ffb054} #hud b.ok{color:#7dd87d}
  .dot{display:inline-block;width:.7em;height:.7em;border-radius:50%;margin-right:.35em}
  .legend span{margin-right:1em}
</style>
<script type="importmap">
{ "imports": { "three":"https://unpkg.com/three@0.160.0/build/three.module.js",
               "three/addons/":"https://unpkg.com/three@0.160.0/examples/jsm/" } }
</script>
</head><body><div id="hud"></div>
<script type="module">
import * as THREE from 'three';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
const DATA = __DATA__;
const renderer = new THREE.WebGLRenderer({ antialias:true });
renderer.setSize(innerWidth, innerHeight); document.body.appendChild(renderer.domElement);
const scene = new THREE.Scene(); scene.background = new THREE.Color(0x15171c);
scene.add(new THREE.AmbientLight(0xffffff, 0.5));
const sun = new THREE.DirectionalLight(0xffffff, 1.2); sun.position.set(2, -1.5, 3); scene.add(sun);
const camera = new THREE.PerspectiveCamera(50, innerWidth/innerHeight, 0.01, 50);
camera.up.set(0,0,1); camera.position.set(1.4,-1.2,1.0);
const controls = new OrbitControls(camera, renderer.domElement); controls.target.set(0,0,0.45);
const grid = new THREE.GridHelper(2,20,0x3a3f4a,0x262a33); grid.rotation.x = Math.PI/2; scene.add(grid);
scene.add(new THREE.AxesHelper(0.25));
const COL = { left:0x4f8fde, right:0x53b97a, fixed:0x8a8f9c };
const UP = new THREE.Vector3(0, 1, 0);
// Each piece is its collision surface: faces offset outward by any radius (caps),
// cylinders along the edges, and spheres at the vertices fill the fillets.
for (const b of DATA.bodies) {
  const color = b.hit ? (DATA.distance <= 0 ? 0xe03c3c : 0xe2902f) : COL[b.side];
  // Faint enough to read the source mesh through the proxy: the gap between the
  // two is the whole point of the scene, so the proxy tints the geometry rather
  // than hiding it.
  const mat = new THREE.MeshStandardMaterial({ color, transparent:true, opacity: b.hit ? 0.35 : 0.20, depthWrite:false, roughness:0.6, side:THREE.DoubleSide });
  if (b.caps.length) {
    const g = new THREE.BufferGeometry();
    g.setAttribute('position', new THREE.Float32BufferAttribute(b.caps, 3));
    g.computeVertexNormals();
    scene.add(new THREE.Mesh(g, mat));
  }
  const nv = b.verts.length / 3;
  if (nv && b.radius > 0) {
    const im = new THREE.InstancedMesh(new THREE.SphereGeometry(b.radius, 12, 8), mat, nv);
    const m = new THREE.Matrix4();
    for (let i = 0; i < nv; i++) { im.setMatrixAt(i, m.makeTranslation(b.verts[3*i], b.verts[3*i+1], b.verts[3*i+2])); }
    im.instanceMatrix.needsUpdate = true;
    scene.add(im);
  }
  const ne = b.edges.length / 6;
  if (ne && b.radius > 0) {
    const im = new THREE.InstancedMesh(new THREE.CylinderGeometry(b.radius, b.radius, 1, 10, 1, true), mat, ne);
    const m = new THREE.Matrix4(), q = new THREE.Quaternion(), a = new THREE.Vector3(), bb = new THREE.Vector3(), d = new THREE.Vector3(), mid = new THREE.Vector3();
    for (let i = 0; i < ne; i++) {
      a.set(b.edges[6*i], b.edges[6*i+1], b.edges[6*i+2]);
      bb.set(b.edges[6*i+3], b.edges[6*i+4], b.edges[6*i+5]);
      d.subVectors(bb, a); const len = d.length(); d.normalize();
      q.setFromUnitVectors(UP, d);
      mid.addVectors(a, bb).multiplyScalar(0.5);
      im.setMatrixAt(i, m.compose(mid, q, new THREE.Vector3(1, len, 1)));
    }
    im.instanceMatrix.needsUpdate = true;
    scene.add(im);
  }
}
for (const w of DATA.meshes) {
  const g = new THREE.BufferGeometry();
  g.setAttribute('position', new THREE.Float32BufferAttribute(w.positions, 3));
  scene.add(new THREE.Mesh(g, new THREE.MeshBasicMaterial({ color:0xc2c8d2, wireframe:true })));
}
{ const [a,b] = DATA.witness.map(p => new THREE.Vector3(...p));
  scene.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([a,b]), new THREE.LineBasicMaterial({ color:0xff5a5a })));
  for (const p of [a,b]) { const d = new THREE.Mesh(new THREE.SphereGeometry(0.008), new THREE.MeshBasicMaterial({ color:0xff5a5a })); d.position.copy(p); scene.add(d); } }
const cls = DATA.distance <= DATA.dStop ? 'bad' : (DATA.distance < DATA.dSafe ? 'warn' : 'ok');
document.getElementById('hud').innerHTML =
  `<div>min distance <b class="${cls}">${DATA.distance.toFixed(4)} m</b> (${DATA.pair})</div>` +
  `<div class="legend"><span><i class="dot" style="background:#4f8fde"></i>left</span>` +
  `<span><i class="dot" style="background:#53b97a"></i>right</span>` +
  `<span><i class="dot" style="background:#8a8f9c"></i>fixed</span>` +
  `<span><i class="dot" style="background:#e2902f"></i>closest pair</span>` +
  `<span>solid = collision surface</span>` +
  `${DATA.meshes.length ? '<span>wireframe = source mesh</span>' : ''}</div>`;
addEventListener('resize', () => { camera.aspect = innerWidth/innerHeight; camera.updateProjectionMatrix(); renderer.setSize(innerWidth, innerHeight); });
renderer.setAnimationLoop(() => { controls.update(); renderer.render(scene, camera); });
</script></body></html>
"##;
