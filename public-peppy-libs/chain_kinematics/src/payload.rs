//! The rigid mass a segment carries beyond its own link: a gripper body, its
//! fingers, a tool, a bracket, a sensor pod. The chain runs through one link per
//! actuated joint, but a dynamics layer must still carry everything bolted to
//! each of them.
//!
//! A link rides on the deepest actuated joint above it, which is the last joint
//! whose motion moves it. That is the whole rule, and it covers the branch past
//! the tip as one case among several: a bracket hanging off the forearm rides on
//! the elbow exactly as a gripper past the wrist rides on the last joint. Taking
//! only the branch below the tip would drop the rest, and a jaw hanging beside
//! the tip frame rather than under it is not an exotic URDF: the SO-101 is
//! written that way.
//!
//! Each segment's extra bodies are lumped into one rigid body, with the joints
//! among them frozen at the URDF home pose. A set of bodies that share no
//! relative motion *is* one rigid body, so the lump is exact for the frozen
//! configuration; the only approximation is ignoring travel on a frozen joint,
//! which is second-order and cancels for a symmetric gripper.
//!
//! The lump is expressed in the segment's own link frame and, at load, folded
//! straight into that segment's inertial ([`Payload::combined_with`]): a bigger
//! segment and a rigidly attached payload are the same rigid body.

use nalgebra::{Isometry3, Matrix3, Point3, Vector3};

use crate::tree::Tree;

/// Lumped distal rigid body, in the tip-link frame. `inertia` is about `com`.
#[derive(Debug, Clone, Copy)]
pub struct Payload {
    pub mass: f64,
    pub com: Vector3<f64>,
    pub inertia: Matrix3<f64>,
}

impl Payload {
    /// No distal mass. Folding it into a segment leaves the segment unchanged.
    pub fn none() -> Self {
        Self {
            mass: 0.0,
            com: Vector3::zeros(),
            inertia: Matrix3::zeros(),
        }
    }

    /// Lump everything riding on `segment` into one rigid body, in the frame of
    /// `link`, that segment's own link. `rides_on` is the attribution from
    /// [`segments_carrying`]. The joints among the lumped bodies are frozen at
    /// zero, which is what makes the lump a single rigid body at all.
    pub fn carried_by(
        tree: &Tree,
        link: usize,
        segment: usize,
        rides_on: &[Option<usize>],
    ) -> Self {
        let bodies: Vec<(f64, Vector3<f64>, Matrix3<f64>)> = tree
            .subtree_from(link, &|_| 0.0)
            .into_iter()
            .filter(|(below, _)| rides_on[*below] == Some(segment))
            .filter_map(|(below, in_segment)| body_in_segment(tree, below, &in_segment))
            .collect();
        compose(&bodies)
    }

    /// Combine this payload with a segment's rigid body, both in that segment's
    /// link frame with each `inertia` about its own COM, returning the merged
    /// body. Used at load to fold the payload into the segment's inertial.
    pub fn combined_with(&self, mass: f64, com: Vector3<f64>, inertia: Matrix3<f64>) -> Payload {
        compose(&[(self.mass, self.com, self.inertia), (mass, com, inertia)])
    }
}

/// Which segment each link rides on: the deepest actuated joint above it, which
/// is the last joint whose motion moves it. `None` for a link above the first
/// actuated joint, which no joint moves and which therefore weighs on none of
/// them. `drives` maps a joint index to the entry of `q` that drives it.
///
/// Indexed by link, so a chain can attribute every link in the URDF once and
/// then ask each segment what it carries.
pub fn segments_carrying(tree: &Tree, base: usize, drives: &[Option<usize>]) -> Vec<Option<usize>> {
    let mut rides_on = vec![None; tree.link_count()];
    let mut stack = vec![(base, None)];
    while let Some((link, carrier)) = stack.pop() {
        for &joint in tree.children_of(link) {
            let below = drives[joint].or(carrier);
            let child = tree.joint(joint).child;
            rides_on[child] = below;
            stack.push((child, below));
        }
    }
    rides_on
}

/// One link's rigid body expressed in a segment's frame:
/// `(mass, COM, inertia about the COM)`, or `None` for a massless link.
fn body_in_segment(
    tree: &Tree,
    link: usize,
    in_segment: &Isometry3<f64>,
) -> Option<(f64, Vector3<f64>, Matrix3<f64>)> {
    let l = tree.link(link);
    if l.mass == 0.0 {
        return None;
    }
    let com = in_segment.transform_point(&Point3::from(l.com)).coords;
    let r = *in_segment.rotation.to_rotation_matrix().matrix();
    Some((l.mass, com, r * l.inertia * r.transpose()))
}

/// Combine several rigid bodies into one: total mass, mass-weighted COM, and the
/// inertia about that COM via the parallel-axis theorem. Each body's `inertia`
/// is already about its own COM, in the shared (tip) frame.
fn compose(bodies: &[(f64, Vector3<f64>, Matrix3<f64>)]) -> Payload {
    let mass: f64 = bodies.iter().map(|(m, _, _)| *m).sum();
    if mass == 0.0 {
        return Payload::none();
    }
    let com = bodies.iter().map(|(m, c, _)| *m * c).sum::<Vector3<f64>>() / mass;

    let mut inertia = Matrix3::zeros();
    for (m, c, i) in bodies {
        // Parallel-axis shift from each body's COM to the composite COM:
        // I += m·(‖d‖²·E − d·dᵀ), d = bodyCOM − compositeCOM.
        let d = c - com;
        inertia += i + *m * (d.dot(&d) * Matrix3::identity() - d * d.transpose());
    }
    Payload { mass, com, inertia }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_none() {
        let p = compose(&[]);
        assert_eq!(p.mass, 0.0);
        assert_eq!(p.com, Vector3::zeros());
        assert_eq!(p.inertia, Matrix3::zeros());
    }

    #[test]
    fn single_body_passes_through() {
        let c = Vector3::new(0.1, -0.2, 0.3);
        let i = Matrix3::from_diagonal(&Vector3::new(1.0, 2.0, 3.0));
        let p = compose(&[(0.5, c, i)]);
        assert!((p.mass - 0.5).abs() < 1e-12);
        assert!((p.com - c).norm() < 1e-12);
        assert!((p.inertia - i).norm() < 1e-12);
    }

    #[test]
    fn two_point_masses_combine_to_known_body() {
        // Two equal point masses (zero own-inertia) at ±x about the origin: COM
        // at the midpoint, and the composite inertia is the parallel-axis sum,
        // a thin dumbbell with zero inertia about its own axis (x).
        let m = 0.5;
        let a = Vector3::new(1.0, 0.0, 0.0);
        let b = Vector3::new(-1.0, 0.0, 0.0);
        let p = compose(&[(m, a, Matrix3::zeros()), (m, b, Matrix3::zeros())]);

        assert!((p.mass - 1.0).abs() < 1e-12);
        assert!(p.com.norm() < 1e-12, "COM = {:?}", p.com);
        // Each mass sits 1 m off the COM along x: d=(±1,0,0), so
        // m·(‖d‖²E − d·dᵀ) = m·diag(0,1,1); summed over both = diag(0,1,1).
        let expected = Matrix3::from_diagonal(&Vector3::new(0.0, 1.0, 1.0));
        assert!((p.inertia - expected).norm() < 1e-12, "I = {:?}", p.inertia);
    }

    #[test]
    fn combined_with_merges_a_segment() {
        // Folding a payload point mass at +x into a segment point mass at -x is
        // the same dumbbell as `two_point_masses_combine_to_known_body`.
        let payload = Payload {
            mass: 0.5,
            com: Vector3::new(1.0, 0.0, 0.0),
            inertia: Matrix3::zeros(),
        };
        let merged = payload.combined_with(0.5, Vector3::new(-1.0, 0.0, 0.0), Matrix3::zeros());
        assert!((merged.mass - 1.0).abs() < 1e-12);
        assert!(merged.com.norm() < 1e-12, "COM = {:?}", merged.com);
        let expected = Matrix3::from_diagonal(&Vector3::new(0.0, 1.0, 1.0));
        assert!(
            (merged.inertia - expected).norm() < 1e-12,
            "I = {:?}",
            merged.inertia
        );
    }
}
