use anyhow;
use peppygen;

fn main() -> anyhow::Result<()> {
    peppygen::run(|messenger| {
        //node.create_timer(std::time::Duration::from_secs(1), || println!("tick"))?;
        Ok(node)
    })
}
