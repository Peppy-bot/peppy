/// Examples:
/// ```
/// const target_tag: &str = "master_node:the_node_instance";
/// let (target_master, target_node) = get_master_node_target(target_tag);
/// assert!(&target_master, "master_node");
/// assert!(&target_node, "the_node_instance");
/// ```
///
/// If there is an env var that already exists in the system, it's given priority.
/// This env var is often set by a deployment.
/// ```
/// const target_instance_id: &str = "the_node";
/// const target_master_node: &str = "remote_master_node";
/// let random_target_tag = "local_node:random";
/// const subscriber_id: &str = "subscriber_1";
///
/// // TODO Fix, not completed, how can get_subscriber_target guess the key of this var?
/// let key = format!("DEPLOYMENT_{}", subscriber_id);
/// unsafe {
///     env::set_var(key, format!("{}:{}", target_master_node, target_instance_id));
/// }
/// let (master_res, target_res) = get_master_node_target(random_target_tag);
/// assert!(&master_res, target_master_node);
/// assert!(&target_res, target_instance_id);
/// ```
pub fn get_subscriber_target(target_tag: &str) -> (String, String) {
    todo!("Finish")
}
