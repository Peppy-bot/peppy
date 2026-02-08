macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

/// Asserts that all given patterns are present in the rendered output.
fn assert_contains_all(rendered: &str, patterns: &[&str]) {
    for pattern in patterns {
        assert_rendered!(
            rendered.contains(pattern),
            rendered,
            "expected to find: {:?}",
            pattern
        );
    }
}

fn render_artifacts(generator: PythonGenerator) -> Vec<String> {
    generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect()
}

fn assert_artifact_contains(artifacts: &[String], pattern: &str) {
    let rendered = artifacts.join("\n");
    assert_rendered!(
        artifacts.iter().any(|artifact| artifact.contains(pattern)),
        &rendered,
        "expected an artifact containing pattern: {:?}",
        pattern
    );
}

mod actions;
mod parameters;
mod services;
mod topics;

use super::*;
