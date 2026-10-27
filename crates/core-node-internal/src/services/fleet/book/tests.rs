use super::*;
use daemon_config::daemon_interface::robot_leases::FleetRobot;

fn client(instance: &str) -> ClientRef {
    ClientRef::new("cn-one", instance)
}

/// A book in which each of `holders` runs a `lease` goal from process `g1`.
fn book_with(holders: &[&str]) -> (FleetBook, BTreeMap<String, HoldingGoal>) {
    let mut book = FleetBook::default();
    let goals = holders
        .iter()
        .map(|holder| {
            let goal = book
                .begin_holding(&client(holder), "g1")
                .expect("the lease goal starts");
            ((*holder).to_owned(), goal)
        })
        .collect();
    (book, goals)
}

fn kind(refusal: &Refusal) -> RefusalKind {
    refusal.kind
}

fn events(goal: &mut HoldingGoal) -> Vec<(LeaseEventKind, String, String)> {
    let mut read = Vec::new();
    while let Ok(event) = goal.events.try_recv() {
        read.push((event.event, event.robot, event.giver));
    }
    read
}

fn changes(follower: &mut UnboundedReceiver<Change>) -> Vec<(ChangeKind, String)> {
    let mut read = Vec::new();
    while let Ok(change) = follower.try_recv() {
        read.push((change.kind, change.robot));
    }
    read
}

fn holder_of(book: &FleetBook, robot: &str) -> Option<String> {
    book.lease_of(robot)
        .map(|(holder, _)| holder.instance_id.clone())
}

#[test]
fn only_a_client_whose_lease_goal_runs_takes_a_lease() {
    let (mut book, _goals) = book_with(&["run"]);
    let refusal = book
        .take(&client("chat"), "alpha", "tidy")
        .expect_err("no lease goal");
    assert_eq!(kind(&refusal), RefusalKind::PreconditionFailed);
    book.take(&client("run"), "alpha", "tidy")
        .expect("a holder takes a free robot");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("run"));
    book.take(&client("run"), "alpha", "tidy")
        .expect("taking a held lease again changes nothing");
}

#[test]
fn a_leased_robot_refuses_another_client_and_names_the_holder() {
    let (mut book, _goals) = book_with(&["run", "headset"]);
    book.take(&client("run"), "alpha", "tidy the table")
        .unwrap();
    let refusal = book
        .take(&client("headset"), "alpha", "teleop")
        .expect_err("leased");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );
    assert!(
        refusal.message.contains("`run` on `cn-one`")
            && refusal.message.contains("`tidy the table`"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_transfer_goes_in_two_steps_with_no_gap_and_comes_back_at_the_release() {
    let (mut book, mut goals) = book_with(&["director", "skill", "other"]);
    book.take(&client("director"), "alpha", "run 7").unwrap();
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .expect("the holder offers");
    assert_eq!(
        events(goals.get_mut("skill").unwrap()),
        [(
            LeaseEventKind::Offer,
            "alpha".to_owned(),
            "director".to_owned()
        )]
    );
    let view = book.listing();
    assert_eq!(
        view.leases[0].receiver, "skill",
        "the listing gives the receiver"
    );

    // During the offer, no third client takes the robot.
    let refusal = book
        .take(&client("other"), "alpha", "x")
        .expect_err("leased");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );

    book.take(&client("skill"), "alpha", "grab")
        .expect("the receiver takes");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("skill"));
    let view = book.listing();
    assert_eq!(view.leases[0].giver, "director");
    assert_eq!(view.leases[0].receiver, "");
    assert_eq!(
        events(goals.get_mut("director").unwrap())
            .into_iter()
            .map(|(event, ..)| event)
            .collect::<Vec<_>>(),
        [LeaseEventKind::LeaseEnd]
    );

    let refusal = book
        .offer(&client("skill"), &["alpha".to_owned()], &client("other"))
        .expect_err("a receiver does not offer again");
    assert_eq!(kind(&refusal), RefusalKind::PreconditionFailed);

    book.release(&client("skill"), "alpha")
        .expect("the receiver releases");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("director"));
    assert_eq!(
        events(goals.get_mut("director").unwrap()),
        [(
            LeaseEventKind::LeaseBack,
            "alpha".to_owned(),
            "skill".to_owned()
        )]
    );
    assert_eq!(
        book.lease_of("alpha").unwrap().1,
        "run 7",
        "the giver's name returns"
    );
}

#[test]
fn a_lease_goes_back_to_its_giver_when_the_receiver_is_gone_and_ends_when_both_are() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    book.end_holding(&client("skill"), "g1", "its process ended");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("director"));

    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    let _skill = book.begin_holding(&client("skill"), "g2").unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    book.end_holding(&client("director"), "g1", "its process ended");
    assert_eq!(
        holder_of(&book, "alpha").as_deref(),
        Some("skill"),
        "the receiver keeps the lease when its giver is gone"
    );
    assert_eq!(book.listing().leases[0].giver, "");
    book.release(&client("skill"), "alpha").unwrap();
    assert_eq!(
        holder_of(&book, "alpha"),
        None,
        "with no giver the lease ends"
    );
}

#[test]
fn an_offer_ends_when_the_receiver_is_gone() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.end_holding(&client("skill"), "g1", "gone");
    assert_eq!(book.listing().leases[0].receiver, "");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("director"));
}

#[test]
fn an_open_lease_lets_any_client_move_and_a_transfer_closes_it() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    let headset = client("headset");
    assert!(book.move_refusal(&headset, Some("alpha")).is_some());
    book.open(&client("director"), "alpha").unwrap();
    assert_eq!(book.move_refusal(&headset, Some("alpha")), None);
    assert_eq!(book.move_refusal(&client("skill"), Some("alpha")), None);
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    assert!(
        !book.listing().leases[0].open,
        "a transfer closes an open lease"
    );
    assert!(book.move_refusal(&headset, Some("alpha")).is_some());
}

#[test]
fn the_move_check_follows_the_leases_and_the_halt() {
    let (mut book, _goals) = book_with(&["director"]);
    let chat = client("chat");
    let director = client("director");
    assert_eq!(
        book.move_refusal(&chat, Some("alpha")),
        None,
        "no lease, no holder"
    );
    assert_eq!(book.move_refusal(&chat, None), None, "outside any copy");
    let refusal = book
        .move_refusal(&director, Some("alpha"))
        .expect("a holder without the lease is refused");
    assert_eq!(refusal.failure_kind, RobotLeasesFailureKind::NotLeased);
    assert!(
        refusal.message.contains("never held the lease"),
        "{}",
        refusal.message
    );

    book.take(&director, "alpha", "run").unwrap();
    assert_eq!(book.move_refusal(&director, Some("alpha")), None);
    let refusal = book.move_refusal(&chat, Some("alpha")).expect("leased");
    assert_eq!(refusal.failure_kind, RobotLeasesFailureKind::Leased);

    book.release(&director, "alpha").unwrap();
    let refusal = book
        .move_refusal(&director, Some("alpha"))
        .expect("released");
    assert_eq!(refusal.failure_kind, RobotLeasesFailureKind::NotLeased);
    assert!(
        refusal.message.contains("released it"),
        "{}",
        refusal.message
    );

    book.halt("chat");
    for (asker, robot) in [
        (&chat, Some("alpha")),
        (&chat, None),
        (&director, Some("beta")),
    ] {
        let refusal = book.move_refusal(asker, robot).expect("halted");
        assert_eq!(refusal.failure_kind, RobotLeasesFailureKind::Halted);
    }
}

#[test]
fn the_halt_ends_every_lease_with_no_give_back_and_refuses_leases_until_cleared() {
    let (mut book, mut goals) = book_with(&["director", "skill"]);
    let mut follower = book.follow().changes;
    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    book.take(&client("director"), "beta", "run").unwrap();
    let _ = changes(&mut follower);
    let _ = events(goals.get_mut("director").unwrap());
    let _ = events(goals.get_mut("skill").unwrap());

    assert!(book.halt("operator"));
    assert_eq!(
        book.listing().leases,
        [],
        "every lease ends, transferred ones too"
    );
    assert_eq!(
        changes(&mut follower),
        [
            (ChangeKind::HaltStart, String::new()),
            (ChangeKind::LeaseEnd, "alpha".to_owned()),
            (ChangeKind::LeaseEnd, "beta".to_owned()),
        ]
    );
    assert_eq!(
        events(goals.get_mut("skill").unwrap())
            .into_iter()
            .map(|(event, robot, _)| (event, robot))
            .collect::<Vec<_>>(),
        [(LeaseEventKind::LeaseEnd, "alpha".to_owned())]
    );
    assert!(!book.halt("operator"), "a second halt changes nothing");
    assert_eq!(changes(&mut follower), []);
    for refusal in [
        book.take(&client("director"), "alpha", "run")
            .expect_err("halted"),
        book.check_join(Some(&client("director")), "gamma")
            .expect_err("a lease holder's join is refused during a halt"),
    ] {
        assert_eq!(
            kind(&refusal),
            RefusalKind::Fleet(RobotLeasesFailureKind::Halted)
        );
    }
    book.check_join(Some(&client("chat")), "gamma")
        .expect("a client that holds no lease joins during a halt");

    assert!(book.clear_halt());
    assert!(!book.clear_halt(), "a clear without a halt changes nothing");
    assert_eq!(
        changes(&mut follower),
        [(ChangeKind::HaltEnd, String::new())]
    );
    assert_eq!(book.listing().leases, [], "the clear starts nothing again");
    book.take(&client("director"), "alpha", "run")
        .expect("leases again");
}

#[test]
fn a_restarted_holder_keeps_no_lease_and_its_new_goal_ends_the_old_one() {
    let (mut book, goals) = book_with(&["headset"]);
    book.take(&client("headset"), "alpha", "teleop").unwrap();
    let refusal = book
        .begin_holding(&client("headset"), "g1")
        .err()
        .expect("one goal per process");
    assert_eq!(kind(&refusal), RefusalKind::PreconditionFailed);
    let _next = book.begin_holding(&client("headset"), "g2").unwrap();
    assert!(goals["headset"].ended.is_cancelled(), "the old goal ends");
    assert_eq!(
        holder_of(&book, "alpha"),
        None,
        "the restart keeps no lease"
    );
    book.end_holding(&client("headset"), "g1", "late end of the old goal");
    book.take(&client("headset"), "alpha", "teleop")
        .expect("the late end leaves the new goal");
}

#[test]
fn a_join_and_a_removal_follow_the_leases() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    let refusal = book
        .check_join(None, "alpha")
        .expect_err("a join under a leased name");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );
    book.check_join(Some(&client("director")), "alpha")
        .expect("the holder joins its leased robot again");

    book.check_join(Some(&client("skill")), "beta").unwrap();
    book.lease_joined(Some(&client("skill")), "beta");
    assert_eq!(
        holder_of(&book, "beta").as_deref(),
        Some("skill"),
        "a holder's join leases"
    );
    book.lease_joined(None, "gamma");
    assert_eq!(
        holder_of(&book, "gamma"),
        None,
        "a join by the command line leases nothing"
    );

    let refusal = book
        .check_removal(None, "alpha")
        .expect_err("the command line removes no leased robot");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );
    book.check_removal(Some(&client("director")), "alpha")
        .expect("the holder removes its robot");
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    book.check_removal(Some(&client("director")), "alpha")
        .expect("the giver removes the robot that it gave");
    book.check_removal(None, "gamma")
        .expect("a robot with no lease");
}

#[test]
fn a_reset_ends_every_lease_and_offer_and_followers_read_each_change_in_order() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    let mut follower = book.follow().changes;
    book.take(&client("director"), "alpha", "run").unwrap();
    book.open(&client("director"), "alpha").unwrap();
    book.close(&client("director"), "alpha").unwrap();
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.note(ChangeKind::Join, "beta");
    book.end_every_lease("the stack was reset");
    assert_eq!(
        changes(&mut follower),
        [
            (ChangeKind::LeaseStart, "alpha".to_owned()),
            (ChangeKind::LeaseOpen, "alpha".to_owned()),
            (ChangeKind::LeaseClose, "alpha".to_owned()),
            (ChangeKind::OfferStart, "alpha".to_owned()),
            (ChangeKind::Join, "beta".to_owned()),
            (ChangeKind::OfferEnd, "alpha".to_owned()),
            (ChangeKind::LeaseEnd, "alpha".to_owned()),
        ]
    );
    assert_eq!(book.listing().leases, []);
    drop(follower);
    book.note(ChangeKind::Join, "gamma");
    assert!(
        book.followers.is_empty(),
        "a follower that went away is dropped"
    );
}

#[test]
fn an_open_close_or_release_of_a_lease_that_the_client_does_not_hold_is_refused() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    for refusal in [
        book.open(&client("skill"), "alpha").expect_err("open"),
        book.close(&client("skill"), "alpha").expect_err("close"),
        book.release(&client("skill"), "alpha")
            .expect_err("release"),
        book.offer(&client("skill"), &["alpha".to_owned()], &client("director"))
            .expect_err("offer"),
    ] {
        assert_eq!(
            kind(&refusal),
            RefusalKind::Fleet(RobotLeasesFailureKind::NotLeased)
        );
    }
}

#[test]
fn the_software_of_a_robot_runs_while_each_of_its_processes_runs_and_each_change_reaches_the_followers()
 {
    let mut book = FleetBook::default();
    let mut follower = book.follow().changes;
    let arm = Process::new("cn-desk", "alpha_arm");
    let camera = Process::new("cn-arm", "alpha_camera");
    let robots = [RobotProcesses {
        robot: "alpha".to_owned(),
        processes: vec![arm.clone(), camera.clone()],
    }];

    book.note_process(arm.clone(), true, &[]);
    book.note_process(camera.clone(), true, &[]);
    book.sync_software(&robots);
    assert_eq!(book.software_of("alpha"), Some(SoftwareState::Running));
    assert!(
        changes(&mut follower).is_empty(),
        "a robot that joined gets its state with no change"
    );

    book.note_process(camera.clone(), false, &robots);
    book.note_process(arm.clone(), false, &robots);
    book.note_process(arm.clone(), true, &robots);
    assert_eq!(
        changes(&mut follower),
        [(ChangeKind::SoftwareStop, "alpha".to_owned())],
        "one stop, and no start while one process of two runs"
    );
    book.set_launch(FleetListing {
        robots: vec![robot_listed("alpha")],
        ..FleetListing::default()
    });
    let mut listings = book.follow().changes;
    book.note_process(arm.clone(), false, &robots);
    book.note_process(arm.clone(), true, &robots);
    assert!(
        listings.try_recv().is_err(),
        "the camera still does not run"
    );
    book.note_process(camera.clone(), true, &robots);
    let started = listings.try_recv().expect("the start of alpha");
    assert_eq!(started.kind, ChangeKind::SoftwareStart);
    assert_eq!(
        started.listing.robots[0].software,
        SoftwareState::Running,
        "the listing of the change gives the state after it"
    );
    book.note_process(camera.clone(), false, &robots);
    let _ = changes(&mut follower);
    book.note_process(Process::new("cn-other", "alpha_camera"), true, &robots);
    assert!(
        changes(&mut follower).is_empty(),
        "an instance of the same id on another machine is another process"
    );
    book.note_process(camera, true, &robots);
    assert_eq!(
        changes(&mut follower),
        [(ChangeKind::SoftwareStart, "alpha".to_owned())]
    );
    assert_eq!(book.software_of("alpha"), Some(SoftwareState::Running));

    book.sync_software(&[]);
    assert_eq!(book.software_of("alpha"), None, "the robot left the stack");
    assert!(changes(&mut follower).is_empty());
}

/// Each lease event of a holder, with its cause.
fn events_with_causes(goal: &mut HoldingGoal) -> Vec<(LeaseEventKind, String, String, String)> {
    let mut read = Vec::new();
    while let Ok(event) = goal.events.try_recv() {
        read.push((event.event, event.robot, event.giver, event.cause));
    }
    read
}

#[test]
fn the_receiver_of_an_offer_hears_each_end_of_it() {
    let (mut book, mut goals) = book_with(&["director", "skill"]);
    let alpha = || vec!["alpha".to_owned()];
    let offer_end_of = |goals: &mut BTreeMap<String, HoldingGoal>| {
        events_with_causes(goals.get_mut("skill").unwrap())
            .into_iter()
            .filter(|(event, ..)| *event == LeaseEventKind::OfferEnd)
            .map(|(_, robot, giver, cause)| (robot, giver, cause))
            .collect::<Vec<_>>()
    };

    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &alpha(), &client("skill"))
        .unwrap();
    book.release(&client("director"), "alpha").unwrap();
    assert_eq!(
        offer_end_of(&mut goals),
        [(
            "alpha".to_owned(),
            "director".to_owned(),
            "`director` on `cn-one` released it".to_owned()
        )],
        "a release by the giver"
    );

    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &alpha(), &client("skill"))
        .unwrap();
    book.halt("operator");
    let ends = offer_end_of(&mut goals);
    assert_eq!(ends.len(), 1, "a halt: {ends:?}");
    assert!(ends[0].2.contains("halted"), "{ends:?}");
    book.clear_halt();

    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &alpha(), &client("skill"))
        .unwrap();
    book.end_holding(&client("director"), "g1", "its process ended");
    let ends = offer_end_of(&mut goals);
    assert_eq!(ends.len(), 1, "the giver is gone: {ends:?}");
    assert!(ends[0].2.contains("its process ended"), "{ends:?}");
}

#[test]
fn an_offer_to_another_receiver_ends_the_offer_before_it() {
    let (mut book, mut goals) = book_with(&["director", "first", "second"]);
    let mut follower = book.follow().changes;
    let alpha = vec!["alpha".to_owned()];
    book.take(&client("director"), "alpha", "run").unwrap();
    book.offer(&client("director"), &alpha, &client("first"))
        .unwrap();
    book.offer(&client("director"), &alpha, &client("first"))
        .expect("an offer to the same receiver again");
    book.offer(&client("director"), &alpha, &client("second"))
        .unwrap();

    assert_eq!(
        events_with_causes(goals.get_mut("first").unwrap()),
        [
            (
                LeaseEventKind::Offer,
                "alpha".to_owned(),
                "director".to_owned(),
                String::new()
            ),
            (
                LeaseEventKind::OfferEnd,
                "alpha".to_owned(),
                "director".to_owned(),
                "`director` on `cn-one` offered the lease to `second` on `cn-one`".to_owned()
            ),
        ],
        "one offer, and its end when the holder offers the lease to another receiver"
    );
    assert_eq!(
        changes(&mut follower),
        [
            (ChangeKind::LeaseStart, "alpha".to_owned()),
            (ChangeKind::OfferStart, "alpha".to_owned()),
            (ChangeKind::OfferEnd, "alpha".to_owned()),
            (ChangeKind::OfferStart, "alpha".to_owned()),
        ],
        "an offer to the same receiver again changes nothing"
    );
    assert_eq!(book.listing().leases[0].receiver, "second");
    let refusal = book
        .take(&client("first"), "alpha", "grab")
        .expect_err("the first receiver no longer has the offer");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );
    book.take(&client("second"), "alpha", "grab")
        .expect("the second receiver takes");
}

#[test]
fn no_client_takes_the_lease_of_a_robot_while_a_removal_of_it_runs() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.begin_removal(None, "alpha")
        .expect("a robot with no lease");
    let refusal = book
        .take(&client("skill"), "alpha", "grab")
        .expect_err("the robot leaves the stack");
    assert_eq!(kind(&refusal), RefusalKind::PreconditionFailed);
    assert!(
        refusal.message.contains("a removal of it runs"),
        "{}",
        refusal.message
    );
    book.take(&client("skill"), "beta", "grab")
        .expect("another robot");
    book.end_removal("alpha");
    book.take(&client("skill"), "alpha", "grab")
        .expect("the removal ended");

    let refusal = book
        .begin_removal(Some(&client("director")), "alpha")
        .expect_err("another client leases the robot");
    assert_eq!(
        kind(&refusal),
        RefusalKind::Fleet(RobotLeasesFailureKind::Leased)
    );
    book.begin_removal(Some(&client("skill")), "alpha")
        .expect("the holder removes its robot");
    book.take(&client("skill"), "alpha", "grab")
        .expect("the holder keeps its lease during its removal");
    book.offer(&client("skill"), &["alpha".to_owned()], &client("director"))
        .unwrap();
    let refusal = book
        .take(&client("director"), "alpha", "run")
        .expect_err("an offer of a robot that leaves the stack is not taken");
    assert_eq!(kind(&refusal), RefusalKind::PreconditionFailed);
}

/// The robot `name` as the launch part of a listing gives it.
fn robot_listed(name: &str) -> FleetRobot {
    FleetRobot {
        name: name.to_owned(),
        option: "so101_sim".to_owned(),
        core_node: "cn-one".to_owned(),
        kind: daemon_config::daemon_interface::robot_leases::RobotKind::Simulated,
        clock: "sim".to_owned(),
        software: SoftwareState::Stopped,
        robot_commander: String::new(),
        instances: Vec::new(),
        contracts: Vec::new(),
    }
}

#[test]
fn each_change_carries_its_revision_and_the_listing_after_it() {
    let (mut book, _goals) = book_with(&["director"]);
    book.set_launch(FleetListing {
        launcher: "desk".to_owned(),
        robots: vec![robot_listed("alpha")],
        ..FleetListing::default()
    });
    let arm = Process::new("cn-one", "alpha_arm");
    book.note_process(
        arm.clone(),
        true,
        &[RobotProcesses {
            robot: "alpha".to_owned(),
            processes: vec![arm],
        }],
    );
    book.take(&client("director"), "alpha", "run").unwrap();
    let Following {
        revision,
        listing,
        mut changes,
    } = book.follow();
    assert_eq!(listing.launcher, "desk", "the launch part of the listing");
    assert_eq!(listing.robots[0].software, SoftwareState::Running);
    assert_eq!(listing.leases[0].holder, "director");
    assert_eq!(
        book.check_move(&client("director"), Some("alpha")).revision,
        revision,
        "the move check reads the revision now"
    );

    // The halt and its clear, before the follower reads either: each change
    // carries the listing after it, not the listing when it is read.
    book.halt("operator");
    book.clear_halt();
    let read: Vec<(ChangeKind, u64, bool, usize)> = std::iter::from_fn(|| changes.try_recv().ok())
        .map(|change| {
            (
                change.kind,
                change.revision,
                change.listing.halted,
                change.listing.leases.len(),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            (ChangeKind::HaltStart, revision + 1, true, 1),
            (ChangeKind::LeaseEnd, revision + 2, true, 0),
            (ChangeKind::HaltEnd, revision + 3, false, 0),
        ]
    );
    assert_eq!(
        book.check_move(&client("director"), None).revision,
        revision + 3
    );
}
