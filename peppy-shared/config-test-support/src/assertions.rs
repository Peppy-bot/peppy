/// Asserts that all given patterns are present in the rendered output.
pub fn assert_contains_all(rendered: &str, patterns: &[&str]) {
    for pattern in patterns {
        if !rendered.contains(pattern) {
            eprintln!("rendered output:\n{}", rendered);
            panic!("expected to find: {:?}", pattern);
        }
    }
}
