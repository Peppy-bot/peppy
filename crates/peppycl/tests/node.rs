use askama::Template;
use std::fs;
mod helpers;

#[derive(Template)]
#[template(path = "simple_node.json5.j2")]
struct SimpleNodeTemplate {
    name: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_setup_node_from_config_file() {
    // Start only the mock messaging router, not the full `peppy serve` process
    let _router = helpers::start_messaging_router()
        .await
        .expect("failed to start messaging router");

    // Render the template into a temporary directory as peppy.json5
    let tmpdir = tempfile::tempdir().expect("failed to create tempdir");
    let tmpl = SimpleNodeTemplate {
        name: "test_node".to_string(),
    };
    let rendered = tmpl.render().expect("failed to render template");
    let config_path = tmpdir.path().join("peppy.json5");
    fs::write(&config_path, rendered).expect("failed to write config file");

    // Invoke setup_node with the generated config file
    let res = peppycl::setup_node(Some(config_path)).await;
    assert!(res.is_ok());

    todo!("Finish")
}
