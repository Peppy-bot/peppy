//! The book of the fleet: who holds the lease of each robot, the offers
//! that run, the halt, the lease holders whose `lease` goal runs, and the
//! clients that follow the changes. Every rule of the leases and the halt
//! lives here, with no messaging, so a test drives it step by step.
//!
//! A lease gives one client the right to move one robot. Only a client
//! whose `lease` goal runs holds leases. A lease ends at its release, at a
//! halt, and when its holder is gone; a lease that its holder received goes
//! back to its giver instead, or ends when the giver is gone. Each change
//! reaches the followers of the fleet in the order the book makes it.
//!
//! The book also keeps the processes of the stack that run, as the daemon
//! of each machine tells, and the software state of each robot: the
//! software of a robot runs when the process of each of its instances runs.

use daemon_config::daemon_interface::fleet::{
    ChangeKind, FleetFailureKind, FleetLease, LeaseEvent, LeaseEventKind, MoveRefusal,
    SoftwareState,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

/// A client of the fleet: an instance, and the machine that runs it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClientRef {
    pub(crate) core_node: String,
    pub(crate) instance_id: String,
}

impl ClientRef {
    pub(crate) fn new(core_node: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
        }
    }
}

impl fmt::Display for ClientRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` on `{}`", self.instance_id, self.core_node)
    }
}

/// Why the book refuses a call: a kind of `fleet:v1`, or
/// `precondition_failed` and `rejected` of peppy, with a message for a
/// person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) kind: RefusalKind,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    Fleet(FleetFailureKind),
    PreconditionFailed,
    Rejected,
}

impl From<MoveRefusal> for Refusal {
    fn from(refusal: MoveRefusal) -> Self {
        Self::fleet(refusal.failure_kind, refusal.message)
    }
}

impl Refusal {
    fn fleet(kind: FleetFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::Fleet(kind),
            message: message.into(),
        }
    }

    fn precondition(message: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::PreconditionFailed,
            message: message.into(),
        }
    }

    pub(crate) fn rejected(message: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::Rejected,
            message: message.into(),
        }
    }
}

/// The client that gave a lease to its holder, and the name it held it
/// under; no client once the giver is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Giver {
    client: Option<ClientRef>,
    name: String,
}

/// The lease of one robot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    holder: ClientRef,
    name: String,
    open: bool,
    /// The giver of a lease that its holder received by an offer.
    received_from: Option<Giver>,
    /// The client that the holder offers the lease to.
    offer: Option<ClientRef>,
}

/// A client whose `lease` goal runs.
struct Holder {
    generation: String,
    events: UnboundedSender<LeaseEvent>,
    /// Cancelled when the book ends the goal: a goal of a later process of
    /// the same instance replaces it.
    ended: CancellationToken,
}

/// What a `lease` goal reads while it runs: the events of its holder, and
/// the token the book cancels when it ends the goal.
pub(crate) struct HoldingGoal {
    pub(crate) events: UnboundedReceiver<LeaseEvent>,
    pub(crate) ended: CancellationToken,
}

/// One change of the fleet, as its followers read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) kind: ChangeKind,
    pub(crate) robot: String,
}

/// The state of the leases, the halt and the software of each robot that
/// the fleet listing gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BookView {
    pub(crate) halted_by: Option<String>,
    pub(crate) leases: Vec<FleetLease>,
    pub(crate) software: BTreeMap<String, SoftwareState>,
}

/// One process of the stack: an instance, and the machine that runs it.
pub(crate) type Process = ClientRef;

/// A robot of the stack, and the processes of its instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RobotProcesses {
    pub(crate) robot: String,
    pub(crate) processes: Vec<Process>,
}

#[derive(Default)]
pub(crate) struct FleetBook {
    /// The client that halted the stack, while it is halted.
    halt: Option<String>,
    leases: BTreeMap<String, Lease>,
    holders: BTreeMap<ClientRef, Holder>,
    /// How the last lease of each client on each robot ended, which a
    /// refusal of `not_leased` gives.
    endings: BTreeMap<(ClientRef, String), String>,
    followers: Vec<UnboundedSender<Change>>,
    /// The processes of the stack that run, on every machine, as the
    /// daemon of each machine told.
    running: BTreeSet<Process>,
    /// The software state of each robot of the stack, as the followers
    /// read it last.
    software: BTreeMap<String, SoftwareState>,
}

impl FleetBook {
    /// A book whose stack the halt `halted_by` holds, as the daemon read it
    /// from disk.
    pub(crate) fn with_halt(halted_by: Option<String>) -> Self {
        Self {
            halt: halted_by,
            ..Self::default()
        }
    }

    /// The client that halted the stack, while it is halted.
    pub(crate) fn halted_by(&self) -> Option<&str> {
        self.halt.as_deref()
    }

    /// The halt and the leases, as the fleet listing gives them.
    pub(crate) fn view(&self) -> BookView {
        BookView {
            halted_by: self.halt.clone(),
            leases: self
                .leases
                .iter()
                .map(|(robot, lease)| {
                    let giver = lease
                        .received_from
                        .as_ref()
                        .and_then(|giver| giver.client.as_ref());
                    FleetLease {
                        robot: robot.clone(),
                        holder: lease.holder.instance_id.clone(),
                        holder_core_node: lease.holder.core_node.clone(),
                        name: lease.name.clone(),
                        open: lease.open,
                        giver: giver.map(|g| g.instance_id.clone()).unwrap_or_default(),
                        giver_core_node: giver.map(|g| g.core_node.clone()).unwrap_or_default(),
                        receiver: lease
                            .offer
                            .as_ref()
                            .map(|r| r.instance_id.clone())
                            .unwrap_or_default(),
                        receiver_core_node: lease
                            .offer
                            .as_ref()
                            .map(|r| r.core_node.clone())
                            .unwrap_or_default(),
                    }
                })
                .collect(),
            software: self.software.clone(),
        }
    }

    /// Notes that `process` runs or that it ended, then the software of
    /// `robots`, the robots of the stack now ([`Self::sync_software`]).
    pub(crate) fn note_process(&mut self, process: Process, runs: bool, robots: &[RobotProcesses]) {
        if runs {
            self.running.insert(process);
        } else {
            self.running.remove(&process);
        }
        self.sync_software(robots);
    }

    /// Reads the software state of each of `robots`, the robots of the
    /// stack now, from the processes that run. The followers read a
    /// software start or stop of each robot whose state changed; a robot
    /// that joined the stack gets its state with no change, as the listing
    /// of its join gives it.
    pub(crate) fn sync_software(&mut self, robots: &[RobotProcesses]) {
        let mut next = BTreeMap::new();
        for robot in robots {
            let runs = !robot.processes.is_empty()
                && robot
                    .processes
                    .iter()
                    .all(|process| self.running.contains(process));
            let state = if runs {
                SoftwareState::Running
            } else {
                SoftwareState::Stopped
            };
            let previous = self.software.get(&robot.robot).copied();
            if previous.is_some_and(|previous| previous != state) {
                let kind = match state {
                    SoftwareState::Running => ChangeKind::SoftwareStart,
                    SoftwareState::Stopped => ChangeKind::SoftwareStop,
                };
                self.note(kind, &robot.robot);
            }
            next.insert(robot.robot.clone(), state);
        }
        self.software = next;
    }

    /// The holder of the lease of `robot` and the name of the lease.
    pub(crate) fn lease_of(&self, robot: &str) -> Option<(&ClientRef, &str)> {
        self.leases
            .get(robot)
            .map(|lease| (&lease.holder, lease.name.as_str()))
    }

    /// Follows the changes of the fleet from now on, in order.
    pub(crate) fn follow(&mut self) -> UnboundedReceiver<Change> {
        let (sender, receiver) = unbounded_channel();
        self.followers.push(sender);
        receiver
    }

    /// Tells every follower of `kind` on `robot`; a follower that went away
    /// is dropped.
    pub(crate) fn note(&mut self, kind: ChangeKind, robot: &str) {
        let change = Change {
            kind,
            robot: robot.to_owned(),
        };
        self.followers
            .retain(|follower| follower.send(change.clone()).is_ok());
    }

    fn tell_holder(&self, client: &ClientRef, event: LeaseEventKind, robot: &str, cause: &str) {
        self.tell_holder_from(client, event, robot, None, cause);
    }

    fn tell_holder_from(
        &self,
        client: &ClientRef,
        event: LeaseEventKind,
        robot: &str,
        giver: Option<&ClientRef>,
        cause: &str,
    ) {
        if let Some(holder) = self.holders.get(client) {
            let _ = holder.events.send(LeaseEvent {
                event,
                robot: robot.to_owned(),
                giver: giver.map(|g| g.instance_id.clone()).unwrap_or_default(),
                giver_core_node: giver.map(|g| g.core_node.clone()).unwrap_or_default(),
                cause: cause.to_owned(),
            });
        }
    }

    /// Starts the `lease` goal of `client` from the process of
    /// `generation`. A goal of the same process that runs refuses it; a goal
    /// of an earlier process of the same instance ends first, as its holder
    /// is gone.
    pub(crate) fn begin_holding(
        &mut self,
        client: &ClientRef,
        generation: &str,
    ) -> Result<HoldingGoal, Refusal> {
        if let Some(holder) = self.holders.get(client) {
            if holder.generation == generation {
                return Err(Refusal::precondition(format!(
                    "a `lease` goal of this process of {client} runs; one goal holds the leases \
                     of a process"
                )));
            }
            let previous = holder.generation.clone();
            self.end_holding(
                client,
                &previous,
                "a later process of the holder started its own lease goal",
            );
        }
        let (events, receiver) = unbounded_channel();
        let ended = CancellationToken::new();
        self.holders.insert(
            client.clone(),
            Holder {
                generation: generation.to_owned(),
                events,
                ended: ended.clone(),
            },
        );
        Ok(HoldingGoal {
            events: receiver,
            ended,
        })
    }

    /// Ends the `lease` goal of `client` from the process of `generation`,
    /// when it is the goal that runs: each lease of the client ends, or goes
    /// back to its giver, and each offer to it or from it ends.
    pub(crate) fn end_holding(&mut self, client: &ClientRef, generation: &str, cause: &str) {
        let Some(holder) = self.holders.get(client) else {
            return;
        };
        if holder.generation != generation {
            return;
        }
        let holder = self
            .holders
            .remove(client)
            .expect("the holder is in the book");
        holder.ended.cancel();
        let gone = format!("its holder {client} is gone: {cause}");
        let robots: Vec<String> = self.leases.keys().cloned().collect();
        for robot in robots {
            let lease = self
                .leases
                .get_mut(&robot)
                .expect("the robot is in the book");
            if lease.offer.as_ref() == Some(client) {
                lease.offer = None;
                self.note(ChangeKind::OfferEnd, &robot);
            }
            let lease = self.leases.get(&robot).expect("the robot is in the book");
            if lease.holder == *client {
                self.give_back_or_end(&robot, &gone);
                continue;
            }
            let giver_gone = lease
                .received_from
                .as_ref()
                .is_some_and(|giver| giver.client.as_ref() == Some(client));
            if giver_gone {
                let lease = self
                    .leases
                    .get_mut(&robot)
                    .expect("the robot is in the book");
                if let Some(giver) = lease.received_from.as_mut() {
                    giver.client = None;
                }
                self.note(ChangeKind::HolderChange, &robot);
            }
        }
    }

    /// Ends the lease of `robot`, or gives it back to the client that gave
    /// it, when that client still holds leases.
    fn give_back_or_end(&mut self, robot: &str, cause: &str) {
        let lease = self.leases.remove(robot).expect("the robot is leased");
        self.endings.insert(
            (lease.holder.clone(), robot.to_owned()),
            format!("its lease ended: {cause}"),
        );
        if lease.offer.is_some() {
            self.note(ChangeKind::OfferEnd, robot);
        }
        let back_to = lease.received_from.as_ref().and_then(|giver| {
            giver
                .client
                .as_ref()
                .filter(|client| self.holders.contains_key(*client))
                .map(|client| (client.clone(), giver.name.clone()))
        });
        let Some((giver, name)) = back_to else {
            self.tell_holder(&lease.holder, LeaseEventKind::LeaseEnd, robot, cause);
            self.note(ChangeKind::LeaseEnd, robot);
            return;
        };
        self.tell_holder(&lease.holder, LeaseEventKind::LeaseEnd, robot, cause);
        self.leases.insert(
            robot.to_owned(),
            Lease {
                holder: giver.clone(),
                name,
                open: false,
                received_from: None,
                offer: None,
            },
        );
        self.tell_holder_from(
            &giver,
            LeaseEventKind::LeaseBack,
            robot,
            Some(&lease.holder),
            cause,
        );
        self.note(ChangeKind::HolderChange, robot);
    }

    fn holder_required(&self, client: &ClientRef) -> Result<(), Refusal> {
        if self.holders.contains_key(client) {
            return Ok(());
        }
        Err(Refusal::precondition(format!(
            "{client} holds no lease goal: a client takes, offers, opens and closes leases while \
             its `lease` goal runs"
        )))
    }

    fn not_halted(&self, what: &str) -> Result<(), Refusal> {
        match &self.halt {
            None => Ok(()),
            Some(by) => Err(Refusal::fleet(
                FleetFailureKind::Halted,
                format!(
                    "the stack is halted (by {by}), so {what} is refused until the halt is cleared"
                ),
            )),
        }
    }

    /// The refusal of `client`, which holds no lease of `robot`, with how
    /// its last lease of the robot ended.
    fn not_leased(&self, client: &ClientRef, robot: &str) -> MoveRefusal {
        let how = self
            .endings
            .get(&(client.clone(), robot.to_owned()))
            .cloned()
            .unwrap_or_else(|| "it never held the lease".to_owned());
        MoveRefusal {
            failure_kind: FleetFailureKind::NotLeased,
            message: format!("{client} holds no lease of robot `{robot}`: {how}"),
        }
    }

    /// The refusal of a client other than the holder of `lease`.
    fn leased_by_another(robot: &str, lease: &Lease) -> MoveRefusal {
        MoveRefusal {
            failure_kind: FleetFailureKind::Leased,
            message: format!(
                "robot `{robot}` is leased by {} under the name `{}`",
                lease.holder, lease.name
            ),
        }
    }

    /// The lease of `robot` that `client` holds.
    fn held(&self, client: &ClientRef, robot: &str) -> Result<&Lease, Refusal> {
        match self.leases.get(robot) {
            Some(lease) if lease.holder == *client => Ok(lease),
            _ => Err(self.not_leased(client, robot).into()),
        }
    }

    /// `client` takes the lease of `robot` under `name`: a robot with no
    /// lease, one that its holder offers to the client, or one that the
    /// client holds already. The caller checks that the robot is on the
    /// stack.
    pub(crate) fn take(
        &mut self,
        client: &ClientRef,
        robot: &str,
        name: &str,
    ) -> Result<(), Refusal> {
        self.holder_required(client)?;
        self.not_halted(&format!("a lease of robot `{robot}`"))?;
        let Some(lease) = self.leases.get(robot) else {
            self.leases.insert(
                robot.to_owned(),
                Lease {
                    holder: client.clone(),
                    name: name.to_owned(),
                    open: false,
                    received_from: None,
                    offer: None,
                },
            );
            self.endings.remove(&(client.clone(), robot.to_owned()));
            self.note(ChangeKind::LeaseStart, robot);
            return Ok(());
        };
        if lease.holder == *client {
            return Ok(());
        }
        if lease.offer.as_ref() != Some(client) {
            return Err(Self::leased_by_another(robot, lease).into());
        }
        let giver = lease.holder.clone();
        let giver_name = lease.name.clone();
        self.leases.insert(
            robot.to_owned(),
            Lease {
                holder: client.clone(),
                name: name.to_owned(),
                open: false,
                received_from: Some(Giver {
                    client: Some(giver.clone()),
                    name: giver_name,
                }),
                offer: None,
            },
        );
        self.endings.remove(&(client.clone(), robot.to_owned()));
        self.tell_holder(
            &giver,
            LeaseEventKind::LeaseEnd,
            robot,
            &format!("{client} took the lease that you offered it"),
        );
        self.note(ChangeKind::OfferEnd, robot);
        self.note(ChangeKind::HolderChange, robot);
        Ok(())
    }

    /// `client` releases its lease of `robot`: a lease that it received goes
    /// back to its giver.
    pub(crate) fn release(&mut self, client: &ClientRef, robot: &str) -> Result<(), Refusal> {
        self.held(client, robot)?;
        self.give_back_or_end(robot, &format!("{client} released it"));
        Ok(())
    }

    /// `client` offers its leases of `robots` to `receiver`. Each lease
    /// stays its own; a lease that the client received is not offered again.
    pub(crate) fn offer(
        &mut self,
        client: &ClientRef,
        robots: &[String],
        receiver: &ClientRef,
    ) -> Result<(), Refusal> {
        self.holder_required(client)?;
        self.not_halted("an offer of a lease")?;
        if robots.is_empty() {
            return Err(Refusal::rejected("an offer names at least one robot"));
        }
        if receiver == client {
            return Err(Refusal::rejected(format!(
                "{client} offers a lease to itself; an offer names another client"
            )));
        }
        for robot in robots {
            let lease = self.held(client, robot)?;
            if lease.received_from.is_some() {
                return Err(Refusal::precondition(format!(
                    "{client} received the lease of robot `{robot}`; it can release it, not \
                     offer it again"
                )));
            }
        }
        for robot in robots {
            let lease = self.leases.get_mut(robot).expect("each robot is held");
            lease.offer = Some(receiver.clone());
            self.tell_holder_from(receiver, LeaseEventKind::Offer, robot, Some(client), "");
            self.note(ChangeKind::OfferStart, robot);
        }
        Ok(())
    }

    /// `client` opens its lease of `robot`: any client may move the robot,
    /// and the client keeps the lease.
    pub(crate) fn open(&mut self, client: &ClientRef, robot: &str) -> Result<(), Refusal> {
        self.not_halted(&format!("opening the lease of robot `{robot}`"))?;
        if self.held(client, robot)?.open {
            return Ok(());
        }
        self.leases.get_mut(robot).expect("the robot is held").open = true;
        self.note(ChangeKind::LeaseOpen, robot);
        Ok(())
    }

    /// `client` closes its lease of `robot`.
    pub(crate) fn close(&mut self, client: &ClientRef, robot: &str) -> Result<(), Refusal> {
        if !self.held(client, robot)?.open {
            return Ok(());
        }
        self.leases.get_mut(robot).expect("the robot is held").open = false;
        self.note(ChangeKind::LeaseClose, robot);
        Ok(())
    }

    /// Whether `client` may move `robot`, the robot of the instance that it
    /// calls or that it names; `None` for an instance outside any copy.
    pub(crate) fn check_move(
        &self,
        client: &ClientRef,
        robot: Option<&str>,
    ) -> Option<MoveRefusal> {
        if let Some(by) = &self.halt {
            return Some(MoveRefusal {
                failure_kind: FleetFailureKind::Halted,
                message: format!(
                    "the stack is halted (by {by}): no move until the halt is cleared"
                ),
            });
        }
        let robot = robot?;
        match self.leases.get(robot) {
            Some(lease) if lease.open || lease.holder == *client => None,
            Some(lease) => Some(Self::leased_by_another(robot, lease)),
            None if self.holders.contains_key(client) => Some(self.not_leased(client, robot)),
            None => None,
        }
    }

    /// Halts the stack for `by`: every lease ends with no give-back, and
    /// every offer with it. Answers whether the halt started now.
    pub(crate) fn halt(&mut self, by: &str) -> bool {
        if self.halt.is_some() {
            return false;
        }
        self.halt = Some(by.to_owned());
        self.note(ChangeKind::HaltStart, "");
        self.end_every_lease(&format!("the stack is halted (by {by})"));
        true
    }

    /// Ends the halt. Answers whether a halt ended.
    pub(crate) fn clear_halt(&mut self) -> bool {
        if self.halt.take().is_none() {
            return false;
        }
        self.note(ChangeKind::HaltEnd, "");
        true
    }

    /// Ends every lease and offer, with no give-back: a halt, a new launch
    /// and a stack reset do.
    pub(crate) fn end_every_lease(&mut self, cause: &str) {
        let leases = std::mem::take(&mut self.leases);
        for (robot, lease) in leases {
            self.endings.insert(
                (lease.holder.clone(), robot.clone()),
                format!("its lease ended: {cause}"),
            );
            if lease.offer.is_some() {
                self.note(ChangeKind::OfferEnd, &robot);
            }
            self.tell_holder(&lease.holder, LeaseEventKind::LeaseEnd, &robot, cause);
            self.note(ChangeKind::LeaseEnd, &robot);
        }
    }

    /// Whether a join of `robot` by `client` may run: refused during a halt
    /// for a lease holder, whose join takes a lease, and under the name of a
    /// robot that another client leases. `None` for a client that is no node
    /// of the stack, such as the command line.
    pub(crate) fn check_join(
        &self,
        client: Option<&ClientRef>,
        robot: &str,
    ) -> Result<(), Refusal> {
        if let Some(client) = client.filter(|client| self.holders.contains_key(*client)) {
            self.not_halted(&format!("the join of robot `{robot}` by a lease holder"))?;
            if let Some(lease) = self.leases.get(robot)
                && lease.holder != *client
            {
                return Err(Self::leased_by_another(robot, lease).into());
            }
            return Ok(());
        }
        match self.leases.get(robot) {
            Some(lease) => Err(Self::leased_by_another(robot, lease).into()),
            None => Ok(()),
        }
    }

    /// Leases `robot`, which `client` joined, to the client when it is a
    /// lease holder, under the name of the robot.
    pub(crate) fn lease_joined(&mut self, client: Option<&ClientRef>, robot: &str) {
        let Some(client) = client.filter(|client| self.holders.contains_key(*client)) else {
            return;
        };
        if self.halt.is_some() || self.leases.contains_key(robot) {
            return;
        }
        self.leases.insert(
            robot.to_owned(),
            Lease {
                holder: client.clone(),
                name: robot.to_owned(),
                open: false,
                received_from: None,
                offer: None,
            },
        );
        self.endings.remove(&(client.clone(), robot.to_owned()));
        self.note(ChangeKind::LeaseStart, robot);
    }

    /// Whether `client` may remove `robot`: only the holder of its lease, or
    /// the giver of that lease, removes a leased robot.
    pub(crate) fn check_removal(
        &self,
        client: Option<&ClientRef>,
        robot: &str,
    ) -> Result<(), Refusal> {
        let Some(lease) = self.leases.get(robot) else {
            return Ok(());
        };
        let giver = lease
            .received_from
            .as_ref()
            .and_then(|giver| giver.client.as_ref());
        if client.is_some_and(|client| lease.holder == *client || giver == Some(client)) {
            return Ok(());
        }
        Err(Self::leased_by_another(robot, lease).into())
    }
}

#[cfg(test)]
mod tests;
