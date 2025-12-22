use anyhow;
use peppygen;

// TODO: Maybe make this async? What is the best, simplest design?
fn main() -> anyhow::Result<()> {
    // spins until shutdown
    // TODO Add `Drop` to `run` to signal the master node that the node has disconnected
    peppygen::run(|messenger| {
        //node.create_timer(std::time::Duration::from_secs(1), || println!("tick"))?;
        Ok(node)
    })
}
