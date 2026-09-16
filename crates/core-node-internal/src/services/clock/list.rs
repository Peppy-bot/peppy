//! The `clock_list` service: what this daemon hosts, and what on it reads a
//! clock.
//!
//! One daemon answers only for itself. `peppy clock list` asks every live
//! daemon and puts the answers together, which is what makes a domain hosted
//! on one machine and read on another visible as the single thing it is.

use std::sync::Arc;

use config::runtime::{ClockDomainId, Name};
use core_node_api::encoding::{
    ClockConsumerInfo, ClockDomainInfo, ClockListRequest, ClockListResponse, LaunchIdentity,
};
use core_node_api::{ServiceId, names};
use node_stack::{InstanceClock, NodeStack};
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;

use super::watch::{ClockWatches, HostedDomain};
use crate::Result;
use crate::services::response::into_service_response;

pub(crate) async fn listen_for_clock_list(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    watches: Arc<ClockWatches>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::ClockList.name(),
    )
    .await?;

    let messenger = messenger.clone();
    let core_node = core_node_node.to_owned();
    let instance = instance_id.to_owned();
    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let node_stack = Arc::clone(&node_stack);
                let watches = Arc::clone(&watches);
                let messenger = messenger.clone();
                let core_node = core_node.clone();
                let instance = instance.clone();
                async move {
                    handle_clock_list(
                        context,
                        &messenger,
                        &core_node,
                        &instance,
                        &node_stack,
                        &watches,
                    )
                    .await
                }
            })
            .await
            .map_err(Into::into)
    });
    Ok(handle)
}

/// One domain hosted here, as the stack records it: the instance supplying it
/// and the launch that started that instance.
struct HostedPublisher {
    domain: ClockDomainId,
    publisher_instance_id: Name,
    launch: Option<LaunchIdentity>,
}

/// Answers with the domains this daemon hosts and the instances on it that
/// read one.
///
/// Both come from the stack: a publisher is a running instance whose binding
/// says so, and a consumer is one that names a domain. The watches supply the
/// one fact the stack cannot, which is whether a domain has actually ticked.
async fn handle_clock_list(
    context: ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_stack: &NodeStack,
    watches: &ClockWatches,
) -> PeppyResult<Payload> {
    if let Err(error) = ClockListRequest::decode(context.message().payload_bytes().as_ref()) {
        return into_service_response(&context, Err(error.into()));
    }

    let (publishers, consumers) = hosted_and_reading(node_stack.instance_clocks());
    watches.prune(
        &publishers
            .iter()
            .map(|hosted| hosted.publisher_instance_id.clone())
            .collect(),
    );
    for hosted in &publishers {
        if let Err(error) = watches
            .ensure(
                messenger,
                core_node_name,
                instance_id,
                &hosted.domain,
                &hosted.publisher_instance_id,
            )
            .await
        {
            // A domain this daemon cannot subscribe to is still hosted here;
            // it reports as not ready rather than vanishing from the listing.
            tracing::warn!(
                %error,
                domain = %hosted.domain,
                "could not watch a hosted clock domain"
            );
        }
    }

    into_service_response(
        &context,
        ClockListResponse {
            domains: domain_listing(&publishers, &watches.snapshot()),
            consumers,
        }
        .encode()
        .map_err(Into::into),
    )
}

/// Splits the stack's clocks into the domains hosted here and the instances
/// reading one. An instance on wall time is neither.
fn hosted_and_reading(
    clocks: Vec<InstanceClock>,
) -> (Vec<HostedPublisher>, Vec<ClockConsumerInfo>) {
    let mut publishers = Vec::new();
    let mut consumers = Vec::new();
    for clock in clocks {
        let Some(domain) = clock.binding.domain().cloned() else {
            continue;
        };
        if clock.binding.is_publisher() {
            publishers.push(HostedPublisher {
                domain,
                publisher_instance_id: clock.instance_id,
                launch: clock.launch,
            });
        } else {
            consumers.push(ClockConsumerInfo {
                instance_id: clock.instance_id.as_str().to_owned(),
                domain,
            });
        }
    }
    (publishers, consumers)
}

/// The listing for the domains this daemon hosts.
///
/// The stack decides which domains exist and which launch owns each one; a
/// watch adds only what it has seen on the wire, so a domain this daemon could
/// not subscribe to is listed and reports as not ready.
fn domain_listing(
    publishers: &[HostedPublisher],
    watched: &[HostedDomain],
) -> Vec<ClockDomainInfo> {
    publishers
        .iter()
        .map(|hosted| {
            let seen = watched.iter().find(|watch| {
                watch.publisher_instance_id == hosted.publisher_instance_id
                    && watch.domain == hosted.domain
            });
            ClockDomainInfo {
                domain: hosted.domain.clone(),
                publisher_instance_id: hosted.publisher_instance_id.as_str().to_owned(),
                launch: hosted.launch.clone(),
                ready: seen.is_some_and(|watch| watch.ready),
                last_tick_ns: seen.and_then(|watch| watch.last_tick_ns),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::{ClockBinding, ClockIncarnation, CoreNodeName, ProducerRef};

    fn domain(name: &str, incarnation: u64) -> ClockDomainId {
        ClockDomainId::new(
            Name::new(name).expect("valid domain name"),
            CoreNodeName::new("cn-sim").expect("valid core node name"),
            ClockIncarnation::try_from(incarnation).expect("non-zero"),
        )
    }

    fn instance_id(id: &str) -> Name {
        Name::new(id).expect("valid instance id")
    }

    fn publisher(
        id: &str,
        domain: &ClockDomainId,
        launch: Option<LaunchIdentity>,
    ) -> InstanceClock {
        InstanceClock {
            instance_id: instance_id(id),
            binding: ClockBinding::publisher(domain.clone()),
            launch,
        }
    }

    fn consumer(id: &str, domain: &ClockDomainId, publisher_id: &str) -> InstanceClock {
        InstanceClock {
            instance_id: instance_id(id),
            binding: ClockBinding::consumer(
                domain.clone(),
                ProducerRef::new("cn-sim", publisher_id),
            ),
            launch: None,
        }
    }

    fn watched(
        domain: &ClockDomainId,
        publisher_id: &str,
        last_tick_ns: Option<u64>,
    ) -> HostedDomain {
        HostedDomain {
            domain: domain.clone(),
            publisher_instance_id: instance_id(publisher_id),
            ready: last_tick_ns.is_some(),
            last_tick_ns,
        }
    }

    /// The stack hosts a domain the moment its publisher is running, so the
    /// listing carries it before any watch has reported it, and an operator
    /// naming it with `--clock` finds it.
    #[test]
    fn a_hosted_domain_is_listed_before_a_watch_reports_it() {
        let robot = domain("robot", 1);
        let listing = domain_listing(
            &[HostedPublisher {
                domain: robot.clone(),
                publisher_instance_id: instance_id("sim_inst"),
                launch: None,
            }],
            &[],
        );

        assert_eq!(listing.len(), 1, "a hosted domain is always listed");
        assert_eq!(listing[0].domain, robot);
        assert_eq!(listing[0].publisher_instance_id, "sim_inst");
        assert!(!listing[0].ready, "a domain no watch has seen is not ready");
        assert_eq!(listing[0].last_tick_ns, None);
    }

    /// Readiness and the last instant come from the domain's own watch. A
    /// watch left from another lifetime of the same name supplies neither,
    /// which is what stops a replacement inheriting a stale instant.
    #[test]
    fn a_domain_reports_the_instant_its_own_watch_saw() {
        let first = domain("robot", 1);
        let second = domain("robot", 2);
        let hosted = [HostedPublisher {
            domain: second.clone(),
            publisher_instance_id: instance_id("sim_inst"),
            launch: None,
        }];

        let stale = domain_listing(&hosted, &[watched(&first, "sim_inst", Some(7))]);
        assert!(
            !stale[0].ready,
            "another lifetime's watch says nothing here"
        );
        assert_eq!(stale[0].last_tick_ns, None);

        let live = domain_listing(&hosted, &[watched(&second, "sim_inst", Some(11))]);
        assert!(live[0].ready);
        assert_eq!(live[0].last_tick_ns, Some(11));
    }

    /// A domain belongs to the launch that started its publisher, which is a
    /// fact about that instance: a domain a `peppy node run` declared reports
    /// none even on a daemon holding a launch's slice.
    #[test]
    fn each_domain_reports_the_launch_that_started_its_publisher() {
        let launched = domain("fleet", 1);
        let typed = domain("bench", 1);
        let identity = LaunchIdentity::new("launch-brave-otter", "cn-sim");
        let (publishers, _) = hosted_and_reading(vec![
            publisher("fleet_inst", &launched, Some(identity.clone())),
            publisher("bench_inst", &typed, None),
        ]);

        let listing = domain_listing(&publishers, &[]);
        let owner = |name: &str| {
            listing
                .iter()
                .find(|info| info.domain.name.as_str() == name)
                .expect("the domain is listed")
                .launch
                .clone()
        };
        assert_eq!(owner("fleet"), Some(identity));
        assert_eq!(
            owner("bench"),
            None,
            "a domain `peppy node run` started belongs to no launch"
        );
    }

    /// The consumers are the instances reading a domain. The instance
    /// supplying it is not one of them, and an instance on wall time reads no
    /// domain at all.
    #[test]
    fn the_consumers_are_the_instances_reading_a_domain() {
        let robot = domain("robot", 1);
        let (publishers, consumers) = hosted_and_reading(vec![
            publisher("sim_inst", &robot, None),
            consumer("arm_inst", &robot, "sim_inst"),
            InstanceClock {
                instance_id: instance_id("viewer_inst"),
                binding: ClockBinding::Wall,
                launch: None,
            },
        ]);

        assert_eq!(publishers.len(), 1);
        assert_eq!(publishers[0].publisher_instance_id.as_str(), "sim_inst");
        assert_eq!(consumers.len(), 1, "wall time is nobody's domain");
        assert_eq!(consumers[0].instance_id, "arm_inst");
        assert_eq!(consumers[0].domain, robot);
    }
}
