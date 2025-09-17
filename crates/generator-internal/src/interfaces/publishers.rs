// TODO: Expose raw messaging interfaces for Python
struct Topics {}

struct Services {}

struct Actions {}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/publishers.rs"
));
