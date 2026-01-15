use peppygen::{NodeBuilder, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().daemon().run(|args: Parameters, node_runner| async {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
