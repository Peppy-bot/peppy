macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

use super::types::InterfaceArtifact;

/// Trait implemented by generators that can produce test artifacts.
pub trait IntoArtifacts {
    fn into_artifacts(self) -> Vec<InterfaceArtifact>;
}

impl IntoArtifacts for super::rust::RustGenerator {
    fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.into_artifacts()
    }
}

impl IntoArtifacts for super::python::PythonGenerator {
    fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.into_artifacts()
    }
}

/// Converts a generator's artifacts into a vector of generated code strings.
pub fn render_artifacts<G: IntoArtifacts>(generator: G) -> Vec<String> {
    generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect()
}

/// Asserts that all given patterns are present in the rendered output.
pub fn assert_contains_all(rendered: &str, patterns: &[&str]) {
    for pattern in patterns {
        assert_rendered!(
            rendered.contains(pattern),
            rendered,
            "expected to find: {:?}",
            pattern
        );
    }
}

/// Asserts that at least one artifact contains the given pattern.
pub fn assert_artifact_contains(artifacts: &[String], pattern: &str) {
    let rendered = artifacts.join("\n");
    assert_rendered!(
        artifacts.iter().any(|artifact| artifact.contains(pattern)),
        &rendered,
        "expected an artifact containing pattern: {:?}",
        pattern
    );
}
