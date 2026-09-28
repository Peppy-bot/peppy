//! Operator-pinned `ZENOH_CONFIG`: `with_router` adopts the pinned file verbatim
//! and reports it as pinned, so the daemon can tell the operator owns the
//! router's federation instead of claiming it.
//!
//! This is an integration test (its own binary) on purpose: it pins a router by
//! setting the process-global `ZENOH_CONFIG`, and the lib's config-render tests
//! read that same var. Isolated here, this test is the only reader/writer of it
//! in its process, so there is no race. Do not add other `ZENOH_CONFIG`-sensitive
//! tests to this file. It exercises the real public path end to end (a
//! hand-pinned config, then `with_router`), rather than poking internal state.

#![cfg(feature = "router")]

use pmi::{RouterId, SubscriberBufferSizes, ZenohAdapter, ZenohNetProtocol, render_router_config};

#[test]
fn a_router_built_under_an_operator_pinned_config_reports_it_and_leaves_it_untouched() {
    let port = 59250;

    // An operator hand-writes a router config and points `ZENOH_CONFIG` at it.
    // `render_router_config` produces exactly the shape zenohd (and the facade's
    // config parser) expect, so this stands in for a real operator-owned file.
    // The operator's own identity for their own router: `with_router`'s
    // `router_id` below is deliberately different, so this test also pins that a
    // pinned config's `id` is never overwritten by the one peppy would have used.
    let operator_id = RouterId::parse("a11ce").expect("a valid router id literal");
    let pinned_config = render_router_config(
        ZenohNetProtocol::Tcp,
        "127.0.0.1",
        port,
        false,
        Vec::new(),
        None,
        &operator_id,
    );
    let cfg_path = std::env::temp_dir().join(format!("peppy_pinned_router_{port}.json5"));
    std::fs::write(&cfg_path, &pinned_config).expect("write the operator-pinned config");

    // SAFETY: this test is the only code in its binary that touches `ZENOH_CONFIG`
    // (the lib's render tests run in a different process), so nothing reads or
    // writes it concurrently.
    unsafe {
        std::env::set_var("ZENOH_CONFIG", &cfg_path);
    }

    // Started the proper way: `with_router` resolves the config via `ZENOH_CONFIG`,
    // adopts the pinned file verbatim, and the facade records that it is pinned.
    let adapter = ZenohAdapter::with_router(
        ZenohNetProtocol::Tcp,
        "127.0.0.1",
        port,
        false,
        SubscriberBufferSizes::default(),
        vec!["tls/rtr.example:7447".to_string()],
        None,
        RouterId::parse("b0b").expect("a valid router id literal"),
    )
    .expect("build a router adapter from the operator-pinned config");

    assert!(
        adapter.router_config_is_pinned(),
        "a router running the ZENOH_CONFIG file verbatim must report itself pinned"
    );
    let after = std::fs::read_to_string(&cfg_path).expect("read the config after with_router");
    assert_eq!(
        pinned_config, after,
        "the operator-pinned config must be left untouched"
    );
    assert!(
        after.contains(operator_id.as_str()) && !after.contains("rtr.example"),
        "the pinned config keeps the operator's own router id and federation, not what peppy was given"
    );

    // A client adapter owns no router and is therefore never pinned.
    let client = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, "127.0.0.1", port)
        .expect("build client adapter");
    assert!(!client.router_config_is_pinned());

    let _ = std::fs::remove_file(&cfg_path);
}
