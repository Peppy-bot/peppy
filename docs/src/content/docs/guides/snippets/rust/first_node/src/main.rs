use peppygen::{NodeBuilder, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(async |args: Parameters, node_runner| {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
