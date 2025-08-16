#[allow(unreachable_code)]
pub fn handle_serve(_host: &str, _zenoh_port: u16) {
    todo!(
        "Run a separate thread that watches changes on peppy.star and updates the pixi envs accordingly"
    );
    todo!("Run a separate thread that starts a Zenoh router");
    todo!(
        "Run a separate thread that listen to Zenoh communication so that it can internally create a map of those communication between nodes"
    );
    todo!(
        "Run a separate thread that is a web server that display the node communication, node list etc..."
    )
}
