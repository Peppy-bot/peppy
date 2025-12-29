use peppygen::{Result, run};

fn main() -> Result<()> {
    run(|args, node_runner| async {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
