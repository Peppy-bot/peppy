//! OpenArm v10 deployment geometry shared by the integration test and the
//! visualizer (included via `#[path]` from both, so it is one source of truth).
//!
//! The torso (`openarm_body_link0`) is concave: a wide base plate, a flared
//! shoulder skirt, a diagonal gusset, a thin central column, and a tapering
//! head. Its auto-fit single hull bridges those into one bulging solid that
//! clips the grippers at rest. These clip regions decompose it instead; each
//! region's slice of the mesh gets the same circumscribing fit a link gets, so
//! every piece tracks the mesh surface. Supply them via [`Builder::regions`] for
//! the torso body.
//!
//! Feature extents measured from the collision mesh (`body_link0_symp.stl`,
//! mm, root frame):
//!
//! ```text
//!  plate   z 0..8, full footprint x [-155, 95], y [-95, 95]
//!  flare   skirt necking y +/-85 (z 12.6) -> +/-30 (z 70), x +/-25
//!  gusset  y +/-30 web from the plate (x -153..-118 at z 8) up a straight
//!          hypotenuse to the column rear (x -30 at z 161..221)
//!  column  x/y +/-30 square (2 mm corner fillets), z 19..604
//!  head    z 603..773; flares to y +/-79 by z 625, x peaks [-85, 65] at
//!          z 695, tapers to about (+/-30, +/-44) at z 770
//! ```
//!
//! Region placement rules (why each bound sits where it does):
//! - Adjacent regions overlap by >= 3 mm. The build check certifies each
//!   ~1 mm face patch inside a single piece, and the mesh has giant wall
//!   triangles crossing every cut, so exact shared cut planes would fail it.
//! - Bounds sit ~1 mm off large flat mesh faces: a region bound resting on
//!   the plate top plane (z = 8 mm) would capture fragments of that whole
//!   face and balloon its piece (the gusset region starts at z = 9 mm).
//! - The gusset region is what makes the decomposition tight: clipped on its
//!   own, its slice hulls to the wedge along the hypotenuse instead of the
//!   old bounding box, whose empty triangle above the diagonal read false
//!   proximity through the workspace behind the column.
//! - The head is banded in z so each piece follows the taper instead of one
//!   box carrying the widest extents over the full 170 mm.

use bimanual_collision_model::ClipRegion;
use bimanual_collision_model::Point3;

const INF: f64 = f64::INFINITY;

/// The collision body whose clip regions replace its auto-fit hull.
pub const TORSO_BODY: &str = "openarm_body_link0";

/// Plate, flare, gusset, column, and four head bands decomposing the OpenArm
/// v10 torso mesh (metres, root frame).
pub fn torso_regions() -> Vec<ClipRegion> {
    [
        // plate: the full-footprint base, everything below the flare skirt.
        ([-INF, -INF, -INF], [INF, INF, 0.012]),
        // flare: skirt + column bottom, fenced off the gusset at x -0.0335.
        ([-0.0335, -0.086, 0.006], [0.033, 0.086, 0.083]),
        // gusset: the diagonal web behind the column; hulls to a tight wedge.
        ([-INF, -0.031, 0.009], [-0.0305, 0.031, 0.226]),
        // column: the square shaft, fenced off the gusset at x -0.0335.
        ([-0.0335, -INF, 0.078], [INF, INF, 0.6025]),
        // head bands: collar and skirt, waist and mid, wide band, top taper.
        ([-INF, -INF, 0.5995], [INF, INF, 0.638]),
        ([-INF, -INF, 0.633], [INF, INF, 0.688]),
        ([-INF, -INF, 0.682], [INF, INF, 0.719]),
        ([-INF, -INF, 0.713], [INF, INF, INF]),
    ]
    .into_iter()
    .map(|(min, max)| {
        ClipRegion::new(
            Point3::new(min[0], min[1], min[2]),
            Point3::new(max[0], max[1], max[2]),
        )
        .expect("torso region bounds are static and valid")
    })
    .collect()
}
