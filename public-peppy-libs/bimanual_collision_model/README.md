# bimanual_collision_model

Runtime self-collision detection for a bimanual robot. Given a pair of joint
configurations, it reports the minimum distance between the two arms, between an
arm and the torso, or between an arm and itself, and it provides a proximity law
for scaling commanded motion as the arms approach contact.

The library is pure Rust with no hardware or messaging dependencies. The model
is built once from a URDF and its collision meshes (the hull fitting takes about
0.7 s in release, and there is no intermediate artifact that can go stale), then
queried every control tick at roughly 0.1 ms per query. The library contains no
robot names: the caller supplies the URDF, the mesh directory, and the two chain
base links. Any bimanual URDF whose arms are 7-DOF SRS chains (the `srs_model`
contract) is supported.

## The proximity band

Two distances define how the model treats clearance, and the caller chooses both
because the geometry alone cannot:

- `d_stop` is the hard floor. At or below it, commanded motion that would
  approach is fully stopped.
- `d_safe` is the full-speed threshold. At or above it, motion runs unthrottled.
  Between `d_safe` and `d_stop`, approaching motion is scaled linearly down to a
  stop.

These two numbers carry the safety buffer that the geometry does not know about:
the worst-case closing speed, the watchdog latency, and the tracking drift of
the controller. The hulls add only a sub-centimetre conservative inflation, so
the band, not the geometry, is where the real margin lives. They are bundled in
a `GovernorBand`, which proves `0 <= d_stop < d_safe` once at construction:

```rust
use bimanual_collision_model::GovernorBand;

// Stop at 10 mm, full speed at or above 30 mm, linear ramp between.
let band = GovernorBand::new(0.01, 0.03)?;
```

On the OpenArm fixture, the closest pair at the rest pose is each arm's base
link against its own forearm (`link0` vs `link3`), about 24 mm apart. That pair
is kinematically blocked from real contact, so the caller excludes it (see
below); with it gone, the closest checked pair at rest is the forearm against
the torso, about 34 mm apart. A 30 mm `d_safe` therefore runs the rest pose at
full speed while still throttling the genuine close approaches the arms reach
elsewhere. The two numbers are the caller's to tune for their robot and
controller.

## Quick start

```rust
use bimanual_collision_model::nalgebra::Point3;
use bimanual_collision_model::{BimanualCollisionModel, ConvexPiece, GovernorBand, PairSpec};

// The proximity law the caller gates with. The model does not hold it: the
// model reports distance, the caller decides what to do with it.
let band = GovernorBand::new(0.01, 0.03)?;

// Tight boxes for a concave body, each verified at build to contain its mesh.
// Axis-aligned here; `ConvexPiece::from_points` takes any convex point cloud.
let torso_boxes = vec![
    ConvexPiece::aabb(Point3::new(-0.157, -0.097, -0.002), Point3::new(0.097, 0.097, 0.022)),
    // ... flare, column, head (see examples/visualize.rs for the full set)
];

let mut model = BimanualCollisionModel::builder_from_file(&urdf_path, &meshes_dir, &left_base, &right_base)?
    // Pairs the caller knows can never collide, dropped from checking (see below).
    .exclude(&[PairSpec::new("openarm_left_link0", "openarm_left_link3")])
    // A tight proxy for a concave body, replacing its single auto-fit hull.
    .hulls("openarm_body_link0", torso_boxes)
    .build()?;

// Watchdog: evaluate the live joint states of both arms.
let p = model.min_distance(&q_left, &q_right)?;
if p.distance <= band.d_stop() {
    // Hold. p.link_a and p.link_b name the offending pair; p.on_a and p.on_b are
    // the closest points in the world frame.
}

// Command vetting: scale a step by where it would land.
let d_now = model.min_distance(&q_left, &q_right)?.distance;
let d_next = model.min_distance(&q_left_next, &q_right)?.distance;
let allowed_fraction = band.scale(d_now, d_next);
```

## Convex hulls, GJK, and the conservative fit

Each collision mesh is wrapped at construction in a convex hull. Every body
auto-fits to a single hull; for a concave body whose one hull would bulge too
far (the torso), the caller supplies a few tight convex pieces instead, verified
at build to contain the mesh. At runtime the only geometry is Gilbert-Johnson-Keerthi
distance between hulls, with the Expanding Polytope Algorithm recovering the
penetration depth and direction where two hulls overlap. The signed distance is
therefore continuous through contact: it falls smoothly through zero rather than
clamping at it, which is what lets the governor distinguish separating motion
from approaching motion even from inside an overlap.

The proxy is strictly conservative, so the reported distance is a true lower
bound on the real mesh distance and the model alarms early, never late. Three
properties combine to guarantee it:

- A convex hull contains its mesh by definition, since every mesh point is a
  convex combination of the hull's own vertices.
- A collision mesh has tens of thousands of vertices, but the hull needs only a
  few hundred, and the runtime support query gets cheaper the fewer there are.
  So before hulling, the cloud is simplified by *welding*: space is divided into
  a grid of cubic cells of side `cell`, and all the points falling in one cell
  are merged to a single representative. This drops the count to a few thousand,
  so the hull builds fast and stays small. Welding moves points (up to a cell
  away from where they were), which could let the hull cut inside the mesh, so
  the hull is then inflated by a `radius` that re-contains every original point.
  That radius is the worst weld displacement plus the worst residual protrusion,
  the latter covering the fact that the incremental hull builder uses
  floating-point predicates and can leave a vertex a hair outside its own faces
  (a defect a repair pass removes and the radius then covers besides). By the
  triangle inequality, and because distance to a convex set is 1-Lipschitz, the
  inflated hull provably contains the mesh.
- Decomposition assigns each whole triangle to exactly one piece, so every
  triangle lands inside its piece's hull and the union of the pieces contains
  the mesh with no face escaping between them.

A stress test confirms it: every fixture mesh point lies inside its body's hull
union, with zero escapes. The crate uses no parry or other heavy collision
framework, because the dependency graph pins nalgebra to 0.30 (through `k`),
which the maintained parry releases have moved past. The hull, GJK, and EPA are
roughly 700 lines of pure nalgebra here.

A bounding-sphere broadphase keeps the per-tick cost down. Each body has a local
bounding sphere; at query time the model places the centres, sorts the candidate
pairs by their lower-bound separation, and stops once that bound exceeds the
running minimum. On the fixture this culls about 88% of pairs before any GJK
runs, and a release-mode test asserts a full query stays under 1 ms.

## Construction: from URDF to hulls

```text
URDF collision entries
  mesh + origin + scale       per-body vertex clouds      (urdf_collision, stl)
  weld + convex hull          rounded hull (hull + radius) (hull::simplified_hull)
  greedy best-plane split     one to a few hull pieces     (hull::decompose)
  attached children           baked into the parent body   (assemble)
  fixed bodies                root-frame hulls
```

- The hull is an incremental convex hull of the welded vertex cloud (`hull.rs`),
  with a repair pass that re-inserts any point the floating-point predicates left
  protruding, so the result is genuinely convex. The convexity matters because
  the support function is a warm-started hill-climb over the hull's edge graph,
  which is only correct on a convex polytope.
- `decompose` greedily splits a body by the plane, swept along the body's
  principal axes, that most reduces the total enclosed volume. It stops once no
  split saves at least an absolute volume threshold. An absolute threshold (not a
  fraction of the body) leaves a small body whole while a large concave one, such
  as the torso, keeps splitting, because the same shape saves far less absolute
  volume when it is small.
- GJK answers distance from a support function alone (`gjk.rs`). On overlap, EPA
  expands the enclosing simplex toward the nearest face of the Minkowski
  difference to recover the penetration depth and direction, reusing the hull's
  horizon-stitching.
- A movable child off a chain link, such as a gripper finger, is baked into the
  parent body's vertex cloud over both travel extremes. Travel is a line in joint
  space, so the two extremes bound every intermediate opening, and the runtime
  needs no gripper state.
- Every collision-bearing link is accounted for. The moving-link names come from
  walking each chain, attached children fold into their parents, and every
  remaining collision link must be world-fixed or construction fails loudly.

## Checked pairs

Which pairs are checked is structural and independent of the geometry. Two
world-fixed bodies are skipped because their distance never changes. Pairs within
two moving joints of each other, whether same-side or the torso against a chain's
first links, are skipped as joint-yoked: the members of such a cluster orbit each
other through their whole range, so their distance swings with every legitimate
motion while real contact between them is blocked by the link in between.
Cross-arm pairs are always checked.

Some pairs the structural rules keep can still never collide over the robot's
actual range (a base link and the upper arm a few joints down, for instance):
checking them only ever throttles spuriously. The `exclude` argument to `new`
drops such pairs. The model trusts the assertion rather than re-deriving it, so
getting the list right is the caller's responsibility, and excluding a pair that
can in fact collide silently removes that protection. Only the names are checked
(they must be real bodies). The dropped pairs are reported by `excluded_pairs`
for the consumer to log. The list is a small, deliberate, reviewed set, computed
from a one-off reachability analysis outside this build.

There is no per-pair rebasing: because the hulls are tight, `min_distance`
reports the raw signed distance, and the band thresholds apply directly.

## The governor law

`band.scale(d_now, d_next)` (on the caller's `GovernorBand`) returns the fraction of a commanded step to
allow, in the range 0 to 1, and it is direction-aware:

- Separating motion, where `d_next > d_now`, always passes at full scale, even
  inside the stop band and even from inside an overlap. This makes a tangled
  state recoverable by an ordinary separating move.
- Approaching motion is scaled by where it would land: 1 at or above `d_safe`,
  0 at or below `d_stop`, and linear in between. A non-finite distance fails safe
  to 0.

This is the model's safety contract: it stops the arms colliding, but it never
stops them moving apart. EPA is what makes the contract hold inside an overlap,
where a hull is still a hair larger than the real part. Plain GJK reports zero on
overlap and cannot tell separating from approaching, but EPA's signed depth can,
so a separating step out of a torso overlap still passes at full speed.

The law sees only the endpoint distances of a step, so steps must stay small
against the band, which holds for per-tick control steps. It is intentionally
discontinuous at `d_next == d_now` inside the band, where barely-separating
motion passes at 1 while hovering motion is scaled.

## What the caller supplies

| Supplied by the caller | Why it cannot be derived |
|---|---|
| URDF path | the robot description itself |
| collision mesh directory | `package://` URIs are not filesystem paths |
| left and right base links | which chain is "left" is robot identity, not geometry |
| `GovernorBand` (`d_stop`, `d_safe`) | the two tuned numbers for how close is too close; they encode worst closing speed, watchdog latency, and tracking drift, none of which the geometry knows |
| excluded pairs (optional) | pairs the caller asserts can never collide, dropped from checking; trusted, not re-derived |

Everything else is derived at construction: the hulls decomposed from the meshes,
the body set, and the checked pairs from the structural rules. Construction fails
loudly on anything it cannot account for (an unreadable mesh, an unmodeled
collision link, an excluded pair naming a body that does not exist), so nothing
is left silently unchecked.

## Calling from the backbone

The consumer is whichever node sees both arms and sits in their bidirectional
joint stream: it reads each arm's measured `joint_states` (keyed by `arm_id`,
0 = left, 1 = right) and produces the `joint_commands` both arms follow. For
OpenArm that is the backbone. It vendors the description (URDF and collision
meshes) and exposes the paths and base links as parameters.

```rust
use bimanual_collision_model::{BimanualCollisionModel, GovernorBand, PairSpec};
use srs_model::JointVec;

// Bringup, once (about 0.7 s in release: hull decomposition and pair derivation).
let band = GovernorBand::new(0.01, 0.03)?;

// On OpenArm, each arm's base shoulder support and its upper arm (link3) come
// within ~12 mm at folded-shoulder poses but cannot touch over the whole joint
// range, so checking the pair only ever throttles spuriously. Drop it on each
// arm. (A sweep of the joints between them confirmed no contact; that analysis
// lives outside this build, which trusts the assertion.)
let exclude = [
    PairSpec::new("openarm_left_link0", "openarm_left_link3"),
    PairSpec::new("openarm_right_link0", "openarm_right_link3"),
];
let mut model = BimanualCollisionModel::from_urdf_file(
    &params.urdf_path,            // the same description the arm nodes load
    &params.collision_meshes_dir,
    &params.left_base_link,       // for example "openarm_left_link0"
    &params.right_base_link,
    &exclude,
)?;
for (a, b) in model.excluded_pairs() {
    // info!("collision: not checking {a} vs {b}");  visible at startup
}

// Latest measured joints of each arm, by arm_id, updated from joint_states.
// Queries take &mut self (forward kinematics in place), so one task owns the
// model; it is Send.
let mut q: [JointVec; 2] = [[0.0; 7]; 2];

// On every joint_states message, keep the arm it addresses:
q[msg.arm_id as usize] = msg.positions;

// Watchdog on the measured state, the backstop. min_distance borrows the model,
// so copy out what must outlive the call.
let here = model.min_distance(&q[0], &q[1])?;
let d_now = here.distance;
if d_now <= band.d_stop() {
    // Hold both arms; here.link_a / here.link_b name the pair, witnesses in root frame.
}

// Streaming to both arms, per command tick: the producer has a target pose `cmd`
// for each arm. Vet the joint step before emitting it. min_distance is over both
// arms at once, so a single scale governs both: move each arm that fraction of
// the way to its target, and the closest pair anywhere paces the whole motion.
let d_next = model.min_distance(&cmd[0], &cmd[1])?.distance;
let scale = band.scale(d_now, d_next);
for arm in [0u8, 1] {
    let i = arm as usize;
    let positions: JointVec = std::array::from_fn(|j| q[i][j] + scale * (cmd[i][j] - q[i][j]));
    let velocities: JointVec = std::array::from_fn(|j| scale * cmd_vel[i][j]);
    // emit joint_commands { arm_id: arm, positions, velocities }
}
```

Separating motion always passes at full scale, so an arm backing out of a tight
spot is never throttled; only motion that would close the gap is slowed, to a
stop at `d_stop`. The hold and abort wiring, and the joint_states / joint_commands
plumbing, live with the backbone, not here.

## Visualization

```sh
cargo run --release --example visualize -- \
    --urdf tests/fixtures/openarm_v10.urdf --meshes tests/fixtures/meshes \
    --left-base openarm_left_link0 --right-base openarm_right_link0 \
    --left 0,0,1.2,0.4,0,0,0 --right 0,0,-1.2,0.4,0,0,0 \
    --wireframes -o scene.html
```

This writes an interactive HTML scene you open in a browser. The solids
are the true rounded collision surface: each piece's faces offset outward by the
inflation radius, with cylinders along the edges and spheres at the vertices
filling the fillets, which is the geometry the distance query actually measures
against. The pieces are coloured by side, and the closest pair is highlighted
with its GJK or EPA witness segment and the signed distance. With `--wireframes`,
the decimated source meshes draw underneath, so the gap between a mesh and its
rounded surface is the conservative margin, visible directly for judging the fit.
`--d-stop` and `--d-safe` set the HUD thresholds.

## Layout

```text
tests/fixtures/                     OpenArm V1.0 fixture, srs_model-style
src/
  hull.rs            convex hull, simplification (weld and radius), decompose
  gjk.rs             GJK distance, EPA penetration, the Hull and Placed primitives
  stl.rs             binary STL reader
  urdf_collision.rs  URDF collision extraction, fixed poses, child transforms
  assemble.rs        construction-time hull decomposition of all bodies
  pairs.rs           pair specs (explicit lists for tests and tools)
  model.rs           BimanualCollisionModel queries, pair derivation, exclusions
  governor.rs        direction-aware proximity scaling
examples/
  visualize.rs       the HTML scene
tests/
  dual_arm.rs        two-arm scenarios: converge, fold, separate, with budgets
```

The fixture meshes are vendored copies of the Enactic `openarm_description` v1.0
collision proxies, and nothing reads outside the crate at build or run time. A
robot with a moving mount, such as a lift axis, needs a model extension, because
fixed bodies are currently baked in the world frame.

## Future work

- **Exclusion verification.** The `exclude` list is trusted today. A useful
  addition would be a diagnostic, run as a test rather than at construction, that
  sweeps each excluded pair across the joints that move the two bodies relative
  to each other and fails if the conservative hulls can reach contact. That turns
  a wrong assertion or a changed mesh into a CI failure without paying a
  per-bringup cost. It only generalizes to narrow kinematic gaps (a few joints);
  a wide gap is exponential to sweep, which is part of why the build trusts the
  list rather than proving it.
- **Moving mounts.** A lift or torso axis would need the currently world-fixed
  bodies posed from a configuration like the arms, rather than baked once.
- **Automatic concave decomposition.** A concave body is handled today by the
  caller supplying tight convex pieces (`Builder::hulls`), verified at build to
  contain the mesh. That is exact but manual. Deriving those pieces automatically
  from the mesh (an approximate convex decomposition, each part then strictly
  bounded) would remove the hand-authoring step while keeping the containment
  guarantee.
- **Other robot topologies.** The "bimanual" and "SRS" assumptions are confined
  to `model.rs`: the two `srs_model::Arm` instances, the `q_left`/`q_right` query,
  the forward-kinematics source, and the same-side and cross-arm pair rules. The
  `hull`, `gjk`, `governor`, `pairs`, `stl`, and `urdf_collision` modules carry no
  robot assumptions. A second topology would lift those modules, plus the generic
  minimum-distance scan in `model.rs`, into a `collision_core` crate, leaving each
  robot crate to pose its bodies from a configuration (today
  `srs_model::Arm::link_pose_world`, the one hard dependency, behind a future
  `BodyPoser` trait) and supply its checked `PairSpec` list. Deliberately not done
  with a single consumer: the second robot defines the seam.

## Testing

`cargo test` covers the hull and GJK analytically (containment of every input
point, re-containment after simplification, box and sphere distances, EPA depth
and direction, signed continuity through contact, and isometry invariance), the
decomposition (a two-lobe shape splits and stays contained), and the fixture
integration scenarios (arms converging monotonically into collision, folding
inward, separating sweeps holding the floor, a governor halt before contact,
separation from inside an overlap passing at full speed, and non-finite
rejection). `cargo test --release` additionally asserts the per-query time budget.
