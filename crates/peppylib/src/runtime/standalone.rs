/// Configuration for running a node in standalone mode (without the peppy daemon).
///
/// This allows running nodes directly with `cargo run` while still having
/// full messaging capabilities with a local Zenoh router.
///
/// # Example
/// ```ignore
/// let config = StandaloneConfig::new("127.0.0.1", 7448, "my_node", "instance_1", my_params);
///
/// run_standalone(config, |params, node_runner| async move {
///     // Use node_runner.messenger() for topics/services
///     Ok(())
/// })
/// ```
#[derive(Debug, Clone)]
pub struct StandaloneConfig<Params> {
    pub(crate) messaging_host: String,
    pub(crate) messaging_port: u16,
    pub(crate) node_name: String,
    pub(crate) instance_id: String,
    pub(crate) master_node: Option<String>,
    pub(crate) parameters: Params,
}

impl<Params> StandaloneConfig<Params> {
    /// Creates a new standalone configuration with required fields.
    ///
    /// The master_node defaults to the node_name if not specified.
    pub fn new(
        messaging_host: impl Into<String>,
        messaging_port: u16,
        node_name: impl Into<String>,
        instance_id: impl Into<String>,
        parameters: Params,
    ) -> Self {
        Self {
            messaging_host: messaging_host.into(),
            messaging_port,
            node_name: node_name.into(),
            instance_id: instance_id.into(),
            master_node: None,
            parameters,
        }
    }

    /// Sets the master node name.
    ///
    /// If not set, defaults to the node_name.
    pub fn with_master_node(mut self, master_node: impl Into<String>) -> Self {
        self.master_node = Some(master_node.into());
        self
    }

    /// Returns the effective master node name (either explicitly set or defaulting to node_name).
    pub(crate) fn effective_master_node(&self) -> &str {
        self.master_node.as_deref().unwrap_or(&self.node_name)
    }
}
