//! Minimal binary STL reader for the construction-time fit and tests.
//!
//! Returns raw triangle vertices (three per facet, in file order); the fit
//! only needs the vertex cloud and the visualization only needs triangles, so
//! no topology reconstruction is done.

use srs_model::nalgebra::Point3;

/// Binary STL layout: 80-byte header, u32 facet count, then per facet a
/// normal and three vertices (4 f32 triples) plus a u16 attribute.
const HEADER_LEN: usize = 80;
const COUNT_LEN: usize = 4;
const FACET_LEN: usize = 50;

/// Parse a binary STL, returning its triangle vertices (length `3 * facets`).
pub fn parse_binary_stl(bytes: &[u8]) -> Result<Vec<Point3<f64>>, String> {
    if bytes.len() < HEADER_LEN + COUNT_LEN {
        return Err(format!("binary STL too short: {} bytes", bytes.len()));
    }
    let count = u32::from_le_bytes(
        bytes[HEADER_LEN..HEADER_LEN + COUNT_LEN]
            .try_into()
            .unwrap(),
    ) as usize;
    // u64 so a hostile facet count cannot wrap the size check on 32-bit.
    let expected = (HEADER_LEN + COUNT_LEN) as u64 + count as u64 * FACET_LEN as u64;
    if bytes.len() as u64 != expected {
        // ASCII STL starts with "solid"; mention it when the size formula fails.
        let hint = if bytes.starts_with(b"solid") {
            " (looks like ASCII STL, only binary is supported)"
        } else {
            ""
        };
        return Err(format!(
            "binary STL size mismatch: {count} facets imply {expected} bytes, file has {}{hint}",
            bytes.len()
        ));
    }

    let mut vertices = Vec::with_capacity(count * 3);
    for facet in 0..count {
        let base = HEADER_LEN + COUNT_LEN + facet * FACET_LEN;
        for v in 0..3 {
            // Skip the 12-byte normal; vertices follow at 12, 24, 36.
            let off = base + 12 + v * 12;
            let coord = |k: usize| {
                f32::from_le_bytes(bytes[off + 4 * k..off + 4 * k + 4].try_into().unwrap()) as f64
            };
            let p = Point3::new(coord(0), coord(1), coord(2));
            if !(p.x.is_finite() && p.y.is_finite() && p.z.is_finite()) {
                return Err(format!("binary STL facet {facet} has a non-finite vertex"));
            }
            vertices.push(p);
        }
    }
    Ok(vertices)
}

/// Read and parse a binary STL file.
pub fn load_stl(path: &str) -> Result<Vec<Point3<f64>>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read stl '{path}': {e}"))?;
    parse_binary_stl(&bytes).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a binary STL with the given triangles (each three vertices).
    fn stl_bytes(triangles: &[[[f32; 3]; 3]]) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_LEN];
        b.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
        for tri in triangles {
            b.extend_from_slice(&[0u8; 12]); // normal, ignored
            for v in tri {
                for c in v {
                    b.extend_from_slice(&c.to_le_bytes());
                }
            }
            b.extend_from_slice(&[0u8; 2]); // attribute
        }
        b
    }

    #[test]
    fn parses_vertices_in_order() {
        let bytes = stl_bytes(&[
            [[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
            [[0., 0., 1.], [1., 0., 1.], [0., 1., 1.]],
        ]);
        let v = parse_binary_stl(&bytes).expect("parse");
        assert_eq!(v.len(), 6);
        assert_eq!(v[0], Point3::new(0., 0., 0.));
        assert_eq!(v[4], Point3::new(1., 0., 1.));
    }

    #[test]
    fn rejects_file_shorter_than_the_fixed_header() {
        let err = parse_binary_stl(&[0u8; 10]).expect_err("too short");
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn rejects_truncated_file() {
        let mut bytes = stl_bytes(&[[[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]]]);
        bytes.truncate(bytes.len() - 10);
        assert!(parse_binary_stl(&bytes).is_err());
    }

    #[test]
    fn rejects_count_mismatch() {
        let mut bytes = stl_bytes(&[[[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]]]);
        bytes[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(parse_binary_stl(&bytes).is_err());
    }

    #[test]
    fn rejects_ascii_with_hint() {
        let mut ascii = b"solid cube\nfacet normal 0 0 1\n".to_vec();
        ascii.extend(std::iter::repeat_n(b' ', 100)); // past the binary header minimum
        let err = parse_binary_stl(&ascii).expect_err("ascii must be rejected");
        assert!(err.contains("ASCII"), "missing hint: {err}");
    }

    #[test]
    fn parses_an_empty_mesh() {
        let v = parse_binary_stl(&stl_bytes(&[])).expect("zero facets is valid");
        assert!(v.is_empty());
    }

    #[test]
    fn rejects_non_finite_vertices() {
        let nan = stl_bytes(&[[[f32::NAN, 0., 0.], [1., 0., 0.], [0., 1., 0.]]]);
        assert!(parse_binary_stl(&nan).is_err());
        let inf = stl_bytes(&[[[1., f32::INFINITY, 0.], [1., 0., 0.], [0., 1., 0.]]]);
        assert!(parse_binary_stl(&inf).is_err());
    }
}
