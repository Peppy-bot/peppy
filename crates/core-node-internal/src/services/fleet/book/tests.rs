use super::*;

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
    assert_eq!(kind(&refusal), RefusalKind::Fleet(FleetFailureKind::Leased));
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
    let view = book.view();
    assert_eq!(
        view.leases[0].receiver, "skill",
        "the listing gives the receiver"
    );

    // During the offer, no third client takes the robot.
    let refusal = book
        .take(&client("other"), "alpha", "x")
        .expect_err("leased");
    assert_eq!(kind(&refusal), RefusalKind::Fleet(FleetFailureKind::Leased));

    book.take(&client("skill"), "alpha", "grab")
        .expect("the receiver takes");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("skill"));
    let view = book.view();
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
    assert_eq!(book.view().leases[0].giver, "");
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
    assert_eq!(book.view().leases[0].receiver, "");
    assert_eq!(holder_of(&book, "alpha").as_deref(), Some("director"));
}

#[test]
fn an_open_lease_lets_any_client_move_and_a_transfer_closes_it() {
    let (mut book, _goals) = book_with(&["director", "skill"]);
    book.take(&client("director"), "alpha", "run").unwrap();
    let headset = client("headset");
    assert!(book.check_move(&headset, Some("alpha")).is_some());
    book.open(&client("director"), "alpha").unwrap();
    assert_eq!(book.check_move(&headset, Some("alpha")), None);
    assert_eq!(book.check_move(&client("skill"), Some("alpha")), None);
    book.offer(&client("director"), &["alpha".to_owned()], &client("skill"))
        .unwrap();
    book.take(&client("skill"), "alpha", "grab").unwrap();
    assert!(
        !book.view().leases[0].open,
        "a transfer closes an open lease"
    );
    assert!(book.check_move(&headset, Some("alpha")).is_some());
}

#[test]
fn the_move_check_follows_the_leases_and_the_halt() {
    let (mut book, _goals) = book_with(&["director"]);
    let chat = client("chat");
    let director = client("director");
    assert_eq!(
        book.check_move(&chat, Some("alpha")),
        None,
        "no lease, no holder"
    );
    assert_eq!(book.check_move(&chat, None), None, "outside any copy");
    let refusal = book
        .check_move(&director, Some("alpha"))
        .expect("a holder without the lease is refused");
    assert_eq!(refusal.failure_kind, FleetFailureKind::NotLeased);
    assert!(
        refusal.message.contains("never held the lease"),
        "{}",
        refusal.message
    );

    book.take(&director, "alpha", "run").unwrap();
    assert_eq!(book.check_move(&director, Some("alpha")), None);
    let refusal = book.check_move(&chat, Some("alpha")).expect("leased");
    assert_eq!(refusal.failure_kind, FleetFailureKind::Leased);

    book.release(&director, "alpha").unwrap();
    let refusal = book.check_move(&director, Some("alpha")).expect("released");
    assert_eq!(refusal.failure_kind, FleetFailureKind::NotLeased);
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
        let refusal = book.check_move(asker, robot).expect("halted");
        assert_eq!(refusal.failure_kind, FleetFailureKind::Halted);
    }
}

#[test]
fn the_halt_ends_every_lease_with_no_give_back_and_refuses_leases_until_cleared() {
    let (mut book, mut goals) = book_with(&["director", "skill"]);
    let mut follower = book.follow();
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
        book.view().leases,
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
        assert_eq!(kind(&refusal), RefusalKind::Fleet(FleetFailureKind::Halted));
    }
    book.check_join(Some(&client("chat")), "gamma")
        .expect("a client that holds no lease joins during a halt");

    assert!(book.clear_halt());
    assert!(!book.clear_halt(), "a clear without a halt changes nothing");
    assert_eq!(
        changes(&mut follower),
        [(ChangeKind::HaltEnd, String::new())]
    );
    assert_eq!(book.view().leases, [], "the clear starts nothing again");
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
    assert_eq!(kind(&refusal), RefusalKind::Fleet(FleetFailureKind::Leased));
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
    assert_eq!(kind(&refusal), RefusalKind::Fleet(FleetFailureKind::Leased));
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
    let mut follower = book.follow();
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
    assert_eq!(book.view().leases, []);
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
            RefusalKind::Fleet(FleetFailureKind::NotLeased)
        );
    }
}
