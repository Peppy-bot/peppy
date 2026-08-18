fn main() -> peppygen::Result<()> {
    peppygen::NodeBuilder::new().run(hello_receiver::setup)
}
