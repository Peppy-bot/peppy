//! Pins the fixture schemas the typed round-trip test compiles at build
//! time to what the schema renderer emits for the fixture formats today.
//!
//! `build.rs` compiles `tests/fixtures/<name>.capnp` into typed accessors;
//! this test proves each of those files is the renderer's output for
//! `tests/fixtures/<name>.json5`, so the accessors the codec is compared
//! against are the ones a generated node gets. Regenerate the schemas with
//! `UPDATE_CODEC_GOLDENS=1 cargo test -p message-codec --test schema_golden`.

use config::node::MessageFormat;
use encoding::MessageFormatMapper;
use std::path::PathBuf;

const FIXTURES: &[&str] = &["everything", "frame"];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

#[test]
fn fixture_schemas_are_what_the_renderer_emits() {
    let update = std::env::var_os("UPDATE_CODEC_GOLDENS").is_some();
    for name in FIXTURES {
        let format_path = fixtures_dir().join(format!("{name}.json5"));
        let schema_path = fixtures_dir().join(format!("{name}.capnp"));
        let format: MessageFormat = serde_json5::from_str(
            &std::fs::read_to_string(&format_path).expect("the fixture format is readable"),
        )
        .expect("the fixture format parses");
        let rendered = MessageFormatMapper::new(name, format)
            .map_message_format_to_capnpn()
            .expect("the fixture format renders")
            .encoding_schema()
            .to_string();
        if update {
            std::fs::write(&schema_path, &rendered).expect("the golden is writable");
            continue;
        }
        let committed = std::fs::read_to_string(&schema_path).unwrap_or_else(|_| {
            panic!(
                "{} is missing; regenerate with UPDATE_CODEC_GOLDENS=1",
                schema_path.display()
            )
        });
        assert_eq!(
            committed,
            rendered,
            "{} differs from the renderer's output; regenerate with UPDATE_CODEC_GOLDENS=1",
            schema_path.display()
        );
    }
}
