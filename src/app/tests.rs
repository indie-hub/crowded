use super::*;

#[test]
fn doorbell_answers_say_exactly_how_far_a_native_message_got() {
    use crate::doorbell::DoorbellEnvelope;
    use native_route::NativeReply::*;
    let answer = |outcome| {
        let (envelope, answers) = DoorbellEnvelope::for_test();
        doorbell_reply(envelope)(7, outcome);
        answers.read()
    };
    assert_eq!(
        answer(Accepted),
        (true, "accepted".to_owned(), Some(7), None)
    );
    assert_eq!(answer(Queued), (true, "queued".to_owned(), Some(7), None));
    assert_eq!(
        answer(Held("why".to_owned())),
        (true, "held".to_owned(), Some(7), None)
    );
    assert_eq!(
        answer(Uncertain),
        (true, "uncertain".to_owned(), Some(7), None)
    );
    assert_eq!(
        answer(Failed("HTTP 401".to_owned())),
        (
            false,
            "rejected".to_owned(),
            None,
            Some("HTTP 401".to_owned())
        )
    );
}

/// Intro, doorbell mail, the Ctrl+W composer and F4 reintroduction are the
/// four ways agent text reaches a room; each must offer a native room to the
/// router before it can fall through to a terminal write.
#[test]
fn all_four_agent_text_entry_points_route_native_rooms_to_the_router() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    assert_eq!(production.matches("router.send(").count(), 4);
    // The terminal writers that remain are the two for ordinary rooms.
    assert_eq!(production.matches(".send_whisper(").count(), 2);
    // The native delivery helper has no terminal writer of its own, so an
    // error there cannot turn into a paste.
    let helper = &production[production.find("fn send_native_mail(").unwrap()..];
    let helper = &helper[..helper.find("\n}\n").unwrap()];
    assert!(!helper.contains(".send_whisper("));
    assert!(!helper.contains(".write_bytes("));
}

/// The session-report path never touches a terminal either: it only
/// swaps router bindings and tells the outbox to hold or adopt mail.
#[test]
fn session_reporting_reaches_no_terminal_writer() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    for name in ["fn report_native_session(", "fn apply_session_report("] {
        let helper = &production[production.find(name).unwrap()..];
        let helper = &helper[..helper.find("\n}\n").unwrap()];
        assert!(!helper.contains(".send_whisper("), "{name} must not paste");
        assert!(!helper.contains(".write_bytes("), "{name} must not paste");
        assert!(!helper.contains("println!"), "{name} stays out of stdout");
    }
}

/// F4 on a native OpenCode room runs exactly the startup intro delivery:
/// the same house-rules text travels through the router, OpenCode accepts
/// it, and each prompt asks for a turn. A second press with an unchanged
/// roster resends the identical text, like the key handler does.
#[cfg(unix)]
#[test]
fn f4_reintroduction_reaches_a_native_room_exactly_like_the_startup_intro() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "a", "v2.2.5");
    let room = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        1,
        &[],
    ))
    .unwrap();
    let entry = RoomEntry {
        index: 0,
        title: "Room A".to_owned(),
        binding: room.binding().clone(),
        backend: Backend::OpenCode(room.server()),
    };
    let path = root.path().join("state").join("native-outbox.json");
    let mut router = NativeRouter::start(path, vec![entry]).unwrap();
    let mut mailroom = Mailroom::new(50);

    let intro = house_rules(1, "Room A (model m, effort unsupported)", 0);
    for _ in 0..2 {
        let (notice, severity) =
            reintroduce_native_room("Room A", &mut router, &mut mailroom, 0, &intro);
        assert_eq!(notice, "Room A reintroduced to the room");
        assert_eq!(severity, NoticeSeverity::Info);
    }

    // The outbox worker saves twice per message and fsyncs file and parent;
    // on a loaded runner that can take far longer than a quiet one, so the
    // bound is generous and the loop still fails a real regression.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        router.tick(Instant::now(), &mut mailroom, false, false);
        let view = mailroom.summary();
        if view.matches("ACCEPTED by OpenCode").count() == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "never accepted:\n{view}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let received = fake.state.received.lock().unwrap().clone();
    let expected = crate::pane::whisper_note("The Crowded Room", &intro);
    assert_eq!(
        received,
        vec![
            (received[0].0.clone(), expected.clone()),
            (received[1].0.clone(), expected),
        ]
    );
    assert_ne!(received[0].0, received[1].0);
    assert_eq!(*fake.state.resumes.lock().unwrap(), [true, true]);
}

#[cfg(all(test, unix))]
pub(crate) fn report_test_stub(
    root: &std::path::Path,
    name: &str,
    log: &std::path::Path,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join(name);
    // As an interactive pane (no arguments) the stub records everything
    // written to the terminal, so a test can prove no report path ever
    // writes to a room. As `codex queue` it answers like the CLI.
    std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = queue ]; then\n  printf '%s\\n' \"$@\" >> '{}'\n  thread=\"\"\n  prev=\"\"\n  for a in \"$@\"; do\n    if [ \"$prev\" = \"--thread\" ]; then thread=\"$a\"; fi\n    prev=\"$a\"\n  done\n  printf 'Queued message sub_1 for thread %s.\\n' \"$thread\"\nelse\n  exec cat > '{}'\nfi\n",
                root.join(format!("{name}.args")).display(),
                log.display()
            ),
        )
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(all(test, unix))]
pub(crate) fn report_test_spec(
    program: &std::path::Path,
    cwd: &std::path::Path,
    title: &str,
    native: bool,
) -> RoomSpec {
    RoomSpec {
        name: "test".to_owned(),
        vendor: "test".to_owned(),
        title: title.to_owned(),
        program: program.as_os_str().to_owned(),
        args: Vec::new(),
        transport: crate::config::Transport::Raw,
        cwd: Some(cwd.to_path_buf()),
        variables: Vec::new(),
        allow_control: true,
        use_headroom: false,
        scheduling: None,
        headroom_args: Vec::new(),
        native_delivery: native,
    }
}

#[cfg(all(test, unix))]
pub(crate) fn report_test_pane(
    spec: RoomSpec,
    native: Option<crate::native_launch::NativeRoom>,
    codex: Option<&pane::CodexMint>,
) -> Pane {
    spawn_room(
        &mut LivePanes,
        spec,
        PtySize {
            rows: 6,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        },
        pane::GuestEnvironment::new([]),
        native,
        codex,
        None,
    )
    .unwrap()
}

#[cfg(all(test, unix))]
fn apply_test_report(
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    from: usize,
    session: Option<&str>,
    pending: bool,
) -> String {
    let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
    let mut notice = Notices::default();
    report_native_session(
        panes,
        router,
        crate::doorbell::DoorbellSessionReport {
            from,
            session: session.map(str::to_owned),
            pending,
            reply,
        },
        &mut notice,
    );
    notice.ordered_lines().join("; ")
}

#[cfg(all(test, unix))]
fn pump_router(router: &mut NativeRouter, mailroom: &mut Mailroom, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        router.tick(Instant::now(), mailroom, false, false);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Ticks the router until `done` holds, so a slow runner cannot fail a test
/// that only needed more time to reach the expected state.
#[cfg(all(test, unix))]
fn pump_router_until(
    router: &mut NativeRouter,
    mailroom: &mut Mailroom,
    done: impl Fn(&Mailroom) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(mailroom) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}:\n{}",
            mailroom.summary()
        );
        router.tick(Instant::now(), mailroom, false, false);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(test, unix))]
pub(crate) fn report_test_opencode_room(
    root: &std::path::Path,
    cwd: &std::path::Path,
    session: &str,
) -> (
    Vec<Pane>,
    Option<NativeRouter>,
    crate::native_launch::tests::FakeRoom,
) {
    use crate::native_launch::{self, tests::fake_room};
    let program = report_test_stub(root, "opencode", &root.join("calls"));
    // The fake is named for the session the tests rebind onto: its
    // echo answers every prompt with ses_b.
    let fake = fake_room(root, "b", "v2.2.5");
    // Consume the launch generation a real startup would have taken, so
    // the report allocates the next one like production does.
    let _launch_generation = crate::native_launch::GenerationAllocator::new(
        crate::pane::session_state::state_directory(),
    )
    .next()
    .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd,
        "1.1".to_owned(),
        1,
        &[],
    ))
    .unwrap();
    let specs = vec![report_test_spec(&program, cwd, "Opencode · 1", true)];
    let server = native.server();
    // The room starts bound to the given session. Tests that need mail to
    // stay unsent use an id the fake server never echoes; tests that
    // deliver use the fake's own session.
    let entry = crate::native_route::RoomEntry {
        index: 0,
        title: "Opencode · 1".to_owned(),
        binding: crate::native_outbox::Binding {
            room: native.binding().room.clone(),
            session: session.to_owned(),
            endpoint: native.binding().endpoint.clone(),
            generation: 1,
        },
        backend: crate::native_route::Backend::OpenCode(server),
    };
    let path = root.join("state").join("native-outbox.json");
    let router = NativeRouter::start(path, vec![entry]).unwrap();
    let panes = vec![report_test_pane(
        specs.into_iter().next().unwrap(),
        Some(native),
        None,
    )];
    (panes, Some(router), fake)
}

#[cfg(all(test, unix))]
#[test]
fn a_pending_report_holds_new_mail_without_sending_or_pasting() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    assert!(notice.contains("session pending after clear"), "{notice}");
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "held",
        None,
    )
    .unwrap();
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            mailroom
                .summary()
                .contains("HELD: session pending after clear")
        },
        "held mail after the pending report",
    );
    assert!(
        fake.state.received.lock().unwrap().is_empty(),
        "nothing is sent while pending"
    );
}

#[cfg(all(test, unix))]
#[test]
fn an_immediate_report_after_pending_delivers_once_paused() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    // Pending, mail, then the new session id with no scheduling pass in
    // between: the hold is already on the mail when the rebind adopts it.
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "held",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    // Paused: nothing moves, but nothing is lost either.
    let end = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, true, false);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fake.state.received.lock().unwrap().is_empty());
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("ACCEPTED"),
        "the native delivery to be accepted",
    );
    let received = fake.state.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
    assert_eq!(received[0].1, "[whisper from Room X] held");
}

#[cfg(all(test, unix))]
#[test]
fn an_immediate_report_after_pending_delivers_once_fused() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "held",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    let end = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, false, true);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fake.state.received.lock().unwrap().is_empty());
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("ACCEPTED"),
        "the native delivery to be accepted",
    );
    let received = fake.state.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
    assert_eq!(received[0].1, "[whisper from Room X] held");
}

#[cfg(all(test, unix))]
#[test]
fn rapid_consecutive_reports_rebind_once_and_deliver_once() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "new",
        None,
    )
    .unwrap();
    let first = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    assert!(
        first.contains("now follows reported session ses_b"),
        "{first}"
    );
    let second = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    assert!(second.contains("already on ses_b"), "{second}");
    let binding = router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(binding.generation, 2, "one rebind, one generation bump");
    // Wait for the delivery itself instead of a fixed budget, then give a
    // duplicate a moment to appear before asserting exactly once.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, false, false);
        if !fake.state.received.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the report mail was never delivered:\n{}",
            mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // A second delivery, if one were coming, would leave on a schedule
    // inside this window.
    std::thread::sleep(Duration::from_millis(400));
    router
        .as_mut()
        .unwrap()
        .tick(Instant::now(), &mut mailroom, false, false);
    let received = fake.state.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
    assert_eq!(received[0].1, "[whisper from Room X] new");
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
}

#[cfg(all(test, unix))]
#[test]
fn the_same_session_id_while_pending_releases_mail_without_a_new_generation() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, _fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "held",
        None,
    )
    .unwrap();
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            mailroom
                .summary()
                .contains("HELD: session pending after clear")
        },
        "the pending hold to be shown",
    );
    assert!(
        mailroom
            .summary()
            .contains("HELD: session pending after clear")
    );
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    assert!(notice.contains("pending cleared"), "{notice}");
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("ACCEPTED by OpenCode"),
        "the held mail to be accepted",
    );
    let view = mailroom.summary();
    assert!(view.contains("ACCEPTED by OpenCode"), "{view}");
    assert_eq!(
        router.as_ref().unwrap().binding_for(0).unwrap().generation,
        1,
        "the same id never bumps the generation"
    );
}

/// Mail saved before the pending signal belongs to the session that
/// ended: an immediate report for that same session must find nothing to
/// release, because no scheduling pass ran between the two commands.
#[cfg(all(test, unix))]
#[test]
fn mail_saved_before_the_pending_signal_is_never_released_by_a_same_id_report() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            mailroom
                .summary()
                .contains("FAILED: sent for an earlier launch")
        },
        "the old mail to be shown failed",
    );
    // A fixed window after the held state, so a stray delivery has time to
    // land before the negative assertion.
    pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
    let received = fake.state.received.lock().unwrap().clone();
    assert!(
        received.is_empty(),
        "mail saved before the pending signal is never delivered: {received:?}"
    );
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
}

/// Two pending signals in a row still leave earlier mail held: the
/// boundary is the first command, so a later report of the same session
/// has nothing to release either.
#[cfg(all(test, unix))]
#[test]
fn mail_saved_before_two_pending_reports_stays_held_when_the_room_returns() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            mailroom
                .summary()
                .contains("FAILED: sent for an earlier launch")
        },
        "the old mail to be shown failed",
    );
    // A fixed window after the held state, so a stray delivery has time to
    // land before the negative assertion.
    pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
    let received = fake.state.received.lock().unwrap().clone();
    assert!(
        received.is_empty(),
        "mail saved before the pending signal is never delivered: {received:?}"
    );
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
}

/// A pending signal followed by a different session keeps earlier mail
/// held while the room moves on: the old session's mail is never resent
/// to the new one.
#[cfg(all(test, unix))]
#[test]
fn mail_saved_before_a_pending_signal_stays_held_when_a_new_session_arrives() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    assert!(
        notice.contains("now follows reported session ses_b"),
        "{notice}"
    );
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            mailroom
                .summary()
                .contains("FAILED: sent for an earlier launch")
        },
        "the old mail to be shown failed",
    );
    // A fixed window after the held state, so a stray delivery has time to
    // land before the negative assertion.
    pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
    let received = fake.state.received.lock().unwrap().clone();
    assert!(
        received.is_empty(),
        "mail saved before the pending signal is never delivered: {received:?}"
    );
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
}

/// The pending report through the production handler while the outbox
/// save fails: the old mail stays held, is never sent and is never
/// marked accepted, even when the same session reports back at once.
#[cfg(all(test, unix))]
#[test]
fn a_failed_pending_save_keeps_old_mail_held_through_the_handler() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    // Drain the enqueue with delivery paused: seeing it queued proves the
    // save landed, and no schedule can have sent the mail yet.
    let settled = Instant::now() + Duration::from_secs(5);
    loop {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, true, false);
        if mailroom.summary().contains("QUEUED") {
            break;
        }
        assert!(Instant::now() < settled, "the enqueue never settled");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The state directory becomes a file, so every later save fails.
    let state_dir = root.path().join("state");
    std::fs::remove_dir_all(&state_dir).unwrap();
    std::fs::write(&state_dir, b"not a directory").unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("HELD"),
        "the old mail to be shown failed",
    );
    // A fixed window after the held state, so a stray delivery has time to
    // land before the negative assertion.
    pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
    let received = fake.state.received.lock().unwrap().clone();
    assert!(
        received.is_empty(),
        "old mail must never be sent while the hold is unsaved: {received:?}"
    );
    let view = mailroom.summary();
    assert!(view.contains("HELD"), "{view}");
    assert!(!view.contains("ACCEPTED"), "{view}");
}

/// Status of one mailroom row, found by its body. Each row is a header followed
/// by its indented body, so this never guesses status from a blanket substring.
#[cfg(all(test, unix))]
fn row_status<'a>(view: &'a str, body: &str) -> Option<&'a str> {
    for block in view.split("\n\n") {
        let mut lines = block.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        if lines.next().is_some_and(|line| line.trim() == body) {
            return header
                .split_once('[')
                .and_then(|(_, status)| status.strip_suffix(']'));
        }
    }
    None
}

/// Ticks until the outbox has answered every command sent so far, which
/// is when nothing is still saving, collecting notices on the way.
#[cfg(all(test, unix))]
fn drain_answered(router: &mut Option<NativeRouter>, mailroom: &mut Mailroom) -> Vec<String> {
    let mut notices = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        notices.extend(
            router
                .as_mut()
                .unwrap()
                .tick(Instant::now(), mailroom, true, false)
                .into_iter()
                .map(|(text, _)| text),
        );
        if !mailroom.summary().contains("SAVING") {
            return notices;
        }
        assert!(
            Instant::now() < deadline,
            "the outbox never answered:\n{}",
            mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Two hold failures waiting for the next production iteration: one from
/// the pending hold write, one from adopting the held mail once the disk
/// is gone. The fake server stays alive for reports that rebind.
#[cfg(all(test, unix))]
fn rig_with_two_hold_failures(
    root: &std::path::Path,
    cwd: &std::path::Path,
) -> (ReportDispatch, crate::native_launch::tests::FakeRoom) {
    let (mut panes, mut router, fake) = report_test_opencode_room(root, cwd, "ses_b");
    let mut mailroom = Mailroom::new(50);
    let state_dir = root.join("state");
    let state_path = state_dir.join("native-outbox.json");

    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    // The pending report cannot write its hold...
    crate::native_outbox::fail_pending_hold_at(&state_path);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    // ...and the mail saved after it is held for the next session. Wait
    // for that save on disk before breaking anything, so there really is
    // held mail for the second failure to hit.
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "new",
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let saved = std::fs::read_to_string(&state_path).unwrap_or_default();
        // The enqueue writes twice: once saved, then released onto the
        // pending hold. Wait for the second, or there is no held mail
        // for the adoption to fail on.
        if saved.contains("] new") && saved.contains("session pending after clear") {
            break;
        }
        assert!(Instant::now() < deadline, "the new mail was never saved");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Now the disk goes away, so adopting that held mail fails too.
    std::fs::remove_dir_all(&state_dir).unwrap();
    std::fs::write(&state_dir, b"not a directory").unwrap();
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    // Give the outbox time to answer everything, so the one iteration
    // below has to carry both failures at once.
    std::thread::sleep(Duration::from_millis(1000));
    (ReportDispatch::new(panes, router), fake)
}

/// Two hold failures drained in one production iteration are both
/// surfaced, in order: none is dropped because another arrived with it.
#[cfg(all(test, unix))]
#[test]
fn two_hold_failures_in_one_iteration_are_both_surfaced() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    let notice = rig.dispatch();
    assert_eq!(
        notice.matches("could not be held").count(),
        2,
        "both hold failures reach the UI in one iteration: {notice}"
    );
    assert!(
        notice.contains("the pending hold could not be written"),
        "the earlier failure is not dropped: {notice}"
    );
}

/// A same-session report processed after the failures adds its own line
/// instead of replacing them.
#[cfg(all(test, unix))]
#[test]
fn two_hold_failures_survive_a_same_session_report_in_one_iteration() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    rig.push_report(0, Some("ses_b"), false);
    let notice = rig.dispatch();
    assert_eq!(
        notice.matches("could not be held").count(),
        2,
        "the report must not replace the failures: {notice}"
    );
    assert!(notice.contains("is already on ses_b"), "{notice}");
}

/// A rebind report processed after the failures keeps them too.
#[cfg(all(test, unix))]
#[test]
fn two_hold_failures_survive_a_rebind_report_in_one_iteration() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    rig.push_report(0, Some("ses-reported-9"), false);
    let notice = rig.dispatch();
    assert_eq!(
        notice.matches("could not be held").count(),
        2,
        "the rebind must not replace the failures: {notice}"
    );
    assert!(
        notice.contains("now follows reported session ses-reported-9"),
        "{notice}"
    );
}

/// A delivery status line processed after the failures keeps them too.
#[cfg(all(test, unix))]
#[test]
fn two_hold_failures_survive_a_delivery_status_in_one_iteration() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    // A message to the room itself: the delivery path writes its own
    // status line after the failures.
    rig.push_message(0, 0, "status line");
    let notice = rig.dispatch();
    assert_eq!(
        notice.matches("could not be held").count(),
        2,
        "the status must not replace the failures: {notice}"
    );
    assert!(
        notice.contains("saved for native delivery"),
        "a delivery status line is there too: {notice}"
    );
    assert!(
        notice.split("; ").count() >= 3,
        "failures and status are all present: {notice}"
    );
}

/// A failure notice survives an unknown-room report in the same
/// iteration, and the report is still surfaced.
#[cfg(all(test, unix))]
#[test]
fn a_failure_notice_survives_an_unknown_room_report_in_one_iteration() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    rig.push_report(9, Some("ses-x"), false);
    let notice = rig.dispatch();
    assert_eq!(
        notice.matches("could not be held").count(),
        2,
        "the unknown room must not replace the failures: {notice}"
    );
    assert!(notice.contains("unknown room"), "{notice}");
}

/// One notice on its own is surfaced unchanged: no separator, nothing
/// added, exactly the line the event produced.
#[cfg(all(test, unix))]
#[test]
fn a_single_notice_is_surfaced_unchanged() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (panes, router, _fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut rig = ReportDispatch::new(panes, router);
    rig.push_report(0, Some("ses_b"), false);
    let notice = rig.dispatch();
    assert_eq!(
        notice, "Opencode · 1 is already on ses_b",
        "a lone notice is exactly as produced"
    );
}

/// Renders the production status line the way the UI does: the actual
/// Paragraph, into an 80-column, one-row buffer.
fn rendered_status(notices: &Notices) -> String {
    rendered_status_at(notices, 80)
}

/// Same as [`rendered_status`], at the given width.
fn rendered_status_at(notices: &Notices, width: u16) -> String {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let area = ratatui::layout::Rect::new(0, 0, width, 1);
    let text = notices.status_line(area.width).expect("a notice to show");
    let mut buffer = Buffer::empty(area);
    Widget::render(Paragraph::new(text), area, &mut buffer);
    (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>()
}

#[test]
fn two_hold_failures_render_a_count_and_a_failure_at_eighty_columns() {
    let mut notices = Notices::default();
    notices.push(
        "native delivery: Opencode · 1 could not be held: the pending hold could not be written"
            .to_owned(),
        NoticeSeverity::Failure,
    );
    notices.push(
        "native delivery: Opencode · 1 could not be held: Not a directory (os error 20)".to_owned(),
        NoticeSeverity::Failure,
    );
    let rendered = rendered_status(&notices);
    assert!(rendered.starts_with("2 failures: "), "{rendered}");
    assert!(rendered.contains("could not be held"), "{rendered}");
}

#[test]
fn a_long_routine_notice_does_not_hide_a_new_failure() {
    let mut notices = Notices::default();
    notices.push(
        format!("Opencode · 1 is already on ses_b — {}", "x".repeat(260)),
        NoticeSeverity::Info,
    );
    notices.push(
        "native delivery: Opencode · 1 could not be held: no space left on the device".to_owned(),
        NoticeSeverity::Failure,
    );
    let rendered = rendered_status(&notices);
    assert!(rendered.starts_with("1 failure: "), "{rendered}");
    assert!(rendered.contains("could not be held"), "{rendered}");
    assert!(
        !rendered.starts_with("Opencode · 1 is already"),
        "the prior routine status must not lead: {rendered}"
    );
}

#[test]
fn one_plain_notice_renders_exactly_as_before() {
    let text = "Opencode · 1 is already on ses_b";
    let mut notices = Notices::default();
    notices.push(text.to_owned(), NoticeSeverity::Info);
    assert_eq!(notices.status_line(80).as_deref(), Some(text));
    let rendered = rendered_status(&notices);
    assert!(rendered.starts_with(text), "{rendered}");
    assert!(!rendered.contains("failure"), "{rendered}");
    assert!(!rendered.contains("notices"), "{rendered}");
}

#[test]
fn one_hundred_reports_stay_bounded_and_show_the_newest_status() {
    let mut notices = Notices::default();
    let mut newest = String::new();
    for index in 0..100 {
        newest = format!("Opencode · 1 is already on ses_b ({index})");
        notices.push(newest.clone(), NoticeSeverity::Info);
    }
    assert!(
        notices.entries.len() <= MAX_NOTICES,
        "the list stays bounded: {}",
        notices.entries.len()
    );
    assert_eq!(
        notices.dropped,
        100 - MAX_NOTICES,
        "the oldest are counted, not forgotten"
    );
    let rendered = rendered_status(&notices);
    assert!(
        rendered.contains(&newest),
        "the newest status is visible: {rendered}"
    );
}

#[test]
fn a_routine_status_never_displaces_a_failure_when_the_list_is_full() {
    let mut notices = Notices::default();
    for index in 0..MAX_NOTICES {
        notices.push(
            format!("native delivery: room {index} could not be held: no space left"),
            NoticeSeverity::Failure,
        );
    }
    notices.push(
        "Opencode · 1 is already on ses_b".to_owned(),
        NoticeSeverity::Info,
    );
    assert_eq!(
        notices
            .entries
            .iter()
            .filter(|entry| entry.severity == NoticeSeverity::Failure)
            .count(),
        MAX_NOTICES,
        "every slot still holds a failure"
    );
    assert_eq!(notices.dropped, 1, "the routine status was the one dropped");
    let rendered = rendered_status(&notices);
    assert!(
        rendered.starts_with(&format!("{MAX_NOTICES} failures: ")),
        "{rendered}"
    );
}

/// The one-row status line stays actionable at every width: a count and
/// the newest failure at 80 and 40 columns, and at 20 columns a count
/// plus an explicit mailroom pointer instead of a bare count.
#[test]
fn status_line_stays_actionable_at_eighty_forty_and_twenty_columns() {
    let mut notices = Notices::default();
    notices.push(
        "Opencode · 1 is already on ses_b".to_owned(),
        NoticeSeverity::Info,
    );
    notices.push(
        "native delivery: Opencode · 2 could not be held: no space left on the device".to_owned(),
        NoticeSeverity::Failure,
    );
    let wide = rendered_status_at(&notices, 80);
    assert!(wide.starts_with("1 failure: "), "{wide}");
    assert!(wide.contains("could not be held"), "{wide}");
    let mid = rendered_status_at(&notices, 40);
    assert!(mid.starts_with("1 failure: "), "{mid}");
    assert!(mid.contains("native delivery"), "{mid}");
    let narrow = rendered_status_at(&notices, 20);
    assert!(narrow.starts_with("1 failure: "), "{narrow}");
    assert!(
        narrow.contains("see mail") || narrow.contains("could not be held"),
        "at 20 columns the row names a reason or points at the mailroom: {narrow}"
    );
    assert!(
        !narrow.trim_end_matches('…').ends_with(':'),
        "never just a bare count: {narrow}"
    );
}

/// Renders the production mailroom popup the way the UI does, through
/// the same render helper the event loop calls, into the real 80x24
/// layout.
fn rendered_popup(mailroom: &Mailroom, notices: &Notices, scroll: u16) -> Vec<String> {
    use ratatui::buffer::Buffer;
    let area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let popup = mail_popup_area(area);
    let mut buffer = Buffer::empty(area);
    render_mail_popup(popup, &mut buffer, mailroom, notices, scroll);
    (popup.y..popup.y + popup.height)
        .map(|y| {
            (popup.x..popup.x + popup.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// With 20 queued envelopes present, every failure is reachable: the
/// popup lists failures first and scrolls, so no failure hides behind
/// the envelopes.
#[test]
fn mailroom_popup_lists_failures_first_and_scrolls_past_envelopes() {
    let mut mailroom = Mailroom::new(100);
    for _ in 0..20 {
        mailroom.queue(
            "Room A".to_owned(),
            "Room B".to_owned(),
            "hi".to_owned(),
            "room busy",
        );
    }
    let mut notices = Notices::default();
    notices.push(
        "Doorbell envelope #0001 injected".to_owned(),
        NoticeSeverity::Info,
    );
    for index in 0..3 {
        notices.push(
            format!("native delivery: Room {index} could not be held: disk {index} full"),
            NoticeSeverity::Failure,
        );
    }
    let text = mail_popup_text(&mailroom, &notices);
    let first_failure = text
        .find("could not be held")
        .expect("a failure in the text");
    let first_info = text
        .find("Doorbell envelope")
        .expect("the info in the text");
    assert!(
        first_failure < first_info,
        "failures sort before routine statuses"
    );
    let top: String = rendered_popup(&mailroom, &notices, 0).join("\n");
    assert!(
        top.contains("could not be held"),
        "a failure is visible without scrolling: {top}"
    );
    for index in 0..3 {
        let needle = format!("disk {index} full");
        let mut seen = false;
        for scroll in (0..60).step_by(5) {
            let view: String = rendered_popup(&mailroom, &notices, scroll).join("\n");
            if view.contains(&needle) {
                seen = true;
                break;
            }
        }
        assert!(seen, "failure {index} is reachable by scrolling");
    }
}

/// F2 opens the Mailroom without discarding anything: the failures the
/// status row pointed at are still in the history and visible in the
/// production popup render after the real toggle transition.
#[test]
fn opening_the_mailroom_keeps_failures_visible_in_the_popup() {
    let mut mailroom = Mailroom::new(100);
    for _ in 0..20 {
        mailroom.queue(
            "Room A".to_owned(),
            "Room B".to_owned(),
            "hi".to_owned(),
            "room busy",
        );
    }
    let mut notices = Notices::default();
    notices.push(
        "native delivery: Room 0 could not be held: disk full".to_owned(),
        NoticeSeverity::Failure,
    );
    notices.push(
        "native delivery: Room 1 could not be held: disk full".to_owned(),
        NoticeSeverity::Failure,
    );
    notices.push(
        "Doorbell envelope #0001 injected".to_owned(),
        NoticeSeverity::Info,
    );
    let mut mode = InputMode::Normal;
    let mut scroll = 7;
    toggle_mail_log(&mut mode, &mut scroll, &mut notices);
    assert!(matches!(mode, InputMode::MailLog));
    assert_eq!(scroll, 0, "opening resets the scroll");
    assert_eq!(notices.entries.len(), 3, "opening keeps the history");
    let view: String = rendered_popup(&mailroom, &notices, scroll).join("\n");
    assert!(
        view.contains("Room 0 could not be held") && view.contains("Room 1 could not be held"),
        "both failures are visible after the F2 press: {view}"
    );
}

/// Leaving the Mailroom by the same toggle dismisses the history: the
/// status row is empty afterwards, like the other view changes that
/// clear (Tab, whisper, settings).
#[test]
fn closing_the_mailroom_dismisses_the_history() {
    let mut notices = Notices::default();
    notices.push(
        "native delivery: Room 0 could not be held: disk full".to_owned(),
        NoticeSeverity::Failure,
    );
    let mut mode = InputMode::MailLog;
    let mut scroll = 4;
    toggle_mail_log(&mut mode, &mut scroll, &mut notices);
    assert!(matches!(mode, InputMode::Normal));
    assert!(notices.entries.is_empty(), "dismissing clears the history");
    assert_eq!(notices.status_line(80), None);
}

/// A repeated routine status renders how many times it arrived, in the
/// status row and in the popup history.
#[test]
fn repeated_routine_status_renders_its_repeat_count() {
    let text = "Opencode · 1 is already on ses_b";
    let mut notices = Notices::default();
    for _ in 0..3 {
        notices.push(text.to_owned(), NoticeSeverity::Info);
    }
    assert_eq!(notices.entries.len(), 1);
    assert_eq!(
        notices.status_line(80).as_deref(),
        Some("1 notices: Opencode · 1 is already on ses_b (x3)")
    );
    assert!(
        notices
            .ordered_lines()
            .iter()
            .any(|line| line.ends_with("(x3)")),
        "the popup keeps the count"
    );
}

/// A repeated failure renders how many times it arrived.
#[test]
fn repeated_failure_renders_its_repeat_count() {
    let text = "native delivery: Room 0 could not be held: disk full";
    let mut notices = Notices::default();
    for _ in 0..3 {
        notices.push(text.to_owned(), NoticeSeverity::Failure);
    }
    assert_eq!(
        notices.status_line(80).as_deref(),
        Some("1 failure: native delivery: Room 0 could not be held: disk full (x3)")
    );
}

/// A successful queued delivery is a routine status: `0 failed` never
/// counts as a failure, so 16 zero-failed statuses cannot displace a
/// real failure. Everything here runs through the production iteration.
#[cfg(all(test, unix))]
#[test]
fn zero_failed_queued_delivery_is_info_and_never_displaces_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    let offline = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        Some(&pane::CodexMint::Failed("gone".to_owned())),
    );
    let online = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room B", false),
        None,
        None,
    );
    let mut rig = ReportDispatch::new(vec![offline, online], None);
    let old = || Instant::now() - Duration::from_secs(60);
    rig.last_output = vec![Some(old()), Some(old())];
    // Sixteen distinct delivery failures through the production route.
    for index in 0..16 {
        rig.push_message(1, 0, &format!("poke {index}"));
        rig.dispatch_keep();
    }
    assert_eq!(rig.notice.entries.len(), 16, "sixteen failures are kept");
    assert!(
        rig.notice
            .entries
            .iter()
            .all(|entry| entry.severity == NoticeSeverity::Failure),
        "every seeded notice is a failure"
    );
    assert_eq!(rig.notice.dropped, 0);
    // One queued status while paused, then one zero-failed delivery
    // status once the room injects it.
    rig.paused = true;
    rig.push_message(0, 1, "hello");
    rig.dispatch_keep();
    rig.paused = false;
    rig.last_output = vec![Some(old()), Some(old())];
    rig.dispatch_keep();
    assert_eq!(rig.notice.entries.len(), 16, "still bounded");
    assert!(
        rig.notice
            .entries
            .iter()
            .all(|entry| entry.severity == NoticeSeverity::Failure),
        "no routine status displaced a failure"
    );
    assert_eq!(rig.notice.dropped, 2, "both routine statuses were dropped");
    assert!(
        !rig.notice
            .entries
            .iter()
            .any(|entry| entry.text.contains("Queued delivery")),
        "a zero-failed status was recorded as routine, not as a failure"
    );
}

/// A queued delivery with a real failure is a failure.
#[cfg(all(test, unix))]
#[test]
fn failed_queued_delivery_is_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    let offline = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        Some(&pane::CodexMint::Failed("gone".to_owned())),
    );
    let mut rig = ReportDispatch::new(vec![offline], None);
    rig.last_output = vec![Some(Instant::now() - Duration::from_secs(60))];
    rig.paused = true;
    rig.push_message(0, 0, "hello");
    rig.dispatch_keep();
    assert!(
        rig.notice
            .entries
            .iter()
            .any(|entry| entry.severity == NoticeSeverity::Info && entry.text.contains("queued")),
        "the queued status is routine"
    );
    rig.paused = false;
    rig.last_output = vec![Some(Instant::now() - Duration::from_secs(60))];
    rig.dispatch_keep();
    let last = rig.notice.entries.last().expect("a delivery status");
    assert!(last.text.contains("1 failed"), "{last:?}");
    assert_eq!(last.severity, NoticeSeverity::Failure);
}

/// The capitalized intro failure is a failure, not a routine status.
#[cfg(all(test, unix))]
#[test]
fn capitalised_teach_failure_is_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    let offline = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        Some(&pane::CodexMint::Failed("gone".to_owned())),
    );
    let mut rig = ReportDispatch::new(vec![offline], None);
    rig.gates[0] = DeliveryGate::new(true);
    rig.spawned_at[0] = Instant::now() - Duration::from_secs(3600);
    rig.dispatch_keep();
    let only = rig.notice.entries.last().expect("an intro notice");
    assert!(only.text.starts_with("Could not teach"), "{only:?}");
    assert_eq!(only.severity, NoticeSeverity::Failure);
}

/// Hold failures are failures; same-session and rebind statuses are
/// routine; an unknown-room report is a failure. All through the
/// production iteration.
#[cfg(all(test, unix))]
#[test]
fn session_notices_carry_producer_severity() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
    rig.dispatch_keep();
    let holds: Vec<_> = rig
        .notice
        .entries
        .iter()
        .filter(|entry| entry.text.contains("could not be held"))
        .collect();
    assert_eq!(holds.len(), 2, "both hold failures reach the UI");
    assert!(
        holds
            .iter()
            .all(|entry| entry.severity == NoticeSeverity::Failure),
        "hold failures are failures"
    );
    rig.push_report(0, Some("ses_b"), false);
    rig.dispatch_keep();
    assert!(
        rig.notice
            .entries
            .iter()
            .any(|entry| entry.severity == NoticeSeverity::Info && entry.text.contains("ses_b")),
        "a same-session status is routine"
    );
    rig.push_report(9, Some("ses-x"), false);
    rig.dispatch_keep();
    let last = rig.notice.entries.last().expect("an unknown-room notice");
    assert!(last.text.contains("unknown room"), "{last:?}");
    assert_eq!(last.severity, NoticeSeverity::Failure);
}

/// Native doorbell outcomes carry their severity: accepted mail is
/// routine, a refused send is a failure.
#[cfg(all(test, unix))]
#[test]
fn deliver_native_paths_carry_their_severity() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    // No router: the send is refused.
    let mut panes = vec![report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        Some(&pane::CodexMint::Failed("gone".to_owned())),
    )];
    let mut mailroom = Mailroom::new(50);
    let mut fuse = DeliveryFuse::new(100);
    let mut paused = false;
    let mut notice = Notices::default();
    deliver_native_doorbell(
        &mut panes,
        None,
        &mut mailroom,
        "Room X",
        "Room A",
        0,
        "hi",
        None,
        &mut fuse,
        &mut paused,
        &mut notice,
    );
    let refused = notice.entries.last().expect("a refusal notice");
    assert!(refused.text.contains("not saved"), "{refused:?}");
    assert_eq!(refused.severity, NoticeSeverity::Failure);
    // A live router accepts the same mail.
    let (mut panes, mut router, _fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let target = panes[0].title().to_owned();
    let mut notice = Notices::default();
    deliver_native_doorbell(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        &target,
        0,
        "hi",
        None,
        &mut fuse,
        &mut paused,
        &mut notice,
    );
    let saved = notice.entries.last().expect("a saved notice");
    assert!(
        saved.text.contains("saved for native delivery"),
        "{saved:?}"
    );
    assert_eq!(saved.severity, NoticeSeverity::Info);
}

/// A reintroduction that the router refuses is a failure (the accepted
/// arm is covered by the F4 test asserting `Info`).
#[cfg(all(test, unix))]
#[test]
fn refused_reintroduction_is_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (_panes, mut router, _fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    let (text, severity) =
        reintroduce_native_room("Ghost", router.as_mut().unwrap(), &mut mailroom, 7, "hi");
    assert!(text.contains("does not use native delivery"), "{text}");
    assert_eq!(severity, NoticeSeverity::Failure);
}

/// Peer control outcomes carry their severity: an applied control is
/// routine, a rejected one is a failure.
#[cfg(all(test, unix))]
#[test]
fn control_outcomes_carry_their_severity() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    use crate::doorbell::{DoorbellControl, DoorbellEvent};
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    // Named `codex` so the room has a control adapter and can resume.
    let stub = report_test_stub(root.path(), "codex", &log);
    let first = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        None,
    );
    let second = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room B", false),
        None,
        None,
    );
    let mut rig = ReportDispatch::new(vec![first, second], None);
    let old = || Instant::now() - Duration::from_secs(60);
    rig.last_output = vec![Some(old()), Some(old())];
    let push_control = |rig: &mut ReportDispatch| {
        let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
        rig.doorbell
            .events
            .push_back(DoorbellEvent::Control(DoorbellControl {
                from: 0,
                to: 1,
                action: crate::doorbell::ControlAction::Resume,
                reply,
            }));
    };
    push_control(&mut rig);
    rig.dispatch_keep();
    let told = rig.notice.entries.last().expect("a control notice");
    assert!(told.text.contains("told"), "{told:?}");
    assert_eq!(told.severity, NoticeSeverity::Info);
    rig.panes[1].set_allow_control(false);
    push_control(&mut rig);
    rig.dispatch_keep();
    let rejected = rig.notice.entries.last().expect("a rejection notice");
    assert!(rejected.text.contains("rejected"), "{rejected:?}");
    assert_eq!(rejected.severity, NoticeSeverity::Failure);
}

/// Fuse notices are failures; an ordinary injected envelope is routine.
/// The room runs a `sleep` stub: it stays online and takes writes, but
/// never produces output, so readiness never flips mid-test.
#[cfg(all(test, unix))]
#[test]
fn fuse_notices_are_failures_and_injected_is_routine() {
    use std::os::unix::fs::PermissionsExt;
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let sleep = root.path().join("room");
    std::fs::write(&sleep, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&sleep, std::fs::Permissions::from_mode(0o700)).unwrap();
    let online = report_test_pane(
        report_test_spec(&sleep, cwd.path(), "Room B", false),
        None,
        None,
    );
    let mut rig = ReportDispatch::new(vec![online], None);
    let old = || Instant::now() - Duration::from_secs(60);
    rig.last_output = vec![Some(old())];
    rig.push_message(0, 0, "first");
    rig.dispatch_keep();
    let injected = rig.notice.entries.last().expect("an injected notice");
    assert!(injected.text.contains("injected"), "{injected:?}");
    assert_eq!(injected.severity, NoticeSeverity::Info);
    // One injection trips a fuse of one; the trip notice is a failure.
    rig.fuse = DeliveryFuse::new(1);
    rig.paused = true;
    rig.push_message(0, 0, "second");
    rig.push_message(0, 0, "third");
    rig.dispatch_keep();
    rig.paused = false;
    rig.last_output = vec![Some(old())];
    rig.gates[0] = DeliveryGate::new(false);
    // The pty echoes the first delivery back on its own schedule; wait
    // until the writer is idle and two passes leave the clock untouched,
    // so no late echo can flip readiness during the inject pass below.
    rig.paused = true;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut quiet_rounds = 0u32;
    while quiet_rounds < 2 {
        assert!(Instant::now() < deadline, "the pty echo never settled");
        rig.last_output = vec![Some(old())];
        let before = rig.last_output[0];
        rig.dispatch_keep();
        if !rig.panes[0].has_unwritten_input() && rig.last_output[0] == before {
            quiet_rounds += 1;
        } else {
            quiet_rounds = 0;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    rig.paused = false;
    rig.gates[0] = DeliveryGate::new(false);
    rig.dispatch_keep();
    let trip = rig.notice.entries.last().expect("a fuse notice");
    assert!(trip.text.contains("fuse tripped"), "{trip:?}");
    assert_eq!(trip.severity, NoticeSeverity::Failure);
}

/// A mint that failed to start the room is a failure.
#[cfg(all(test, unix))]
#[test]
fn failed_mint_is_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    let online = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room B", false),
        None,
        None,
    );
    let mut rig = ReportDispatch::new(vec![online], None);
    let (tx, rx) = std::sync::mpsc::channel();
    drop(tx);
    rig.codex[0] = Some(pane::CodexMint::Pending(rx));
    rig.dispatch_keep();
    let mint = rig
        .notice
        .entries
        .iter()
        .find(|entry| entry.text.contains("was not started"))
        .expect("a mint notice");
    assert_eq!(mint.severity, NoticeSeverity::Failure);
}

/// A rejected key is a failure, chosen where the text is produced.
#[cfg(all(test, unix))]
#[test]
fn forward_key_rejection_is_a_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();

    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let log = root.path().join("writes");
    let stub = report_test_stub(root.path(), "room", &log);
    let mut pane = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Room A", false),
        None,
        Some(&pane::CodexMint::Failed("gone".to_owned())),
    );
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    );
    let (text, severity) = forward_key(&mut pane, key).expect("a rejection");
    assert!(text.contains("rejected input"), "{text}");
    assert_eq!(severity, NoticeSeverity::Failure);
}

/// A pending-hold write failure belongs to the pending command: it shows
/// up as a notice naming the room, and the mail queued around it keeps
/// its own result instead of inheriting that failure.
#[cfg(all(test, unix))]
#[test]
fn a_pending_save_failure_is_reported_as_its_own_failure() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    let state_path = root.path().join("state").join("native-outbox.json");

    // Old mail, the pending report, then new mail with no pass in
    // between: the outbox answers the three commands in that order.
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    crate::native_outbox::fail_pending_hold_at(&state_path);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "new",
        None,
    )
    .unwrap();

    let notices = drain_answered(&mut router, &mut mailroom);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            let view = mailroom.summary();
            row_status(&view, "old")
                == Some("FAILED: sent for an earlier launch of this room; not delivered")
        },
        "the stale row to settle",
    );
    let view = mailroom.summary();
    assert_eq!(
        view.lines()
            .filter(|line| line.contains("[FAILED:"))
            .count(),
        1,
        "only the stale row is failed: {view}"
    );
    assert_eq!(
        row_status(&view, "old"),
        Some("FAILED: sent for an earlier launch of this room; not delivered"),
        "{view}"
    );
    assert_eq!(
        view.matches("sent for an earlier launch of this room; not delivered")
            .count(),
        1,
        "the stale reason appears once: {view}"
    );
    assert!(
        row_status(&view, "new").is_some_and(|status| !status.contains("FAILED")),
        "the new mail keeps its own result: {view}"
    );
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("could not be held") && notice.contains("Opencode · 1")),
        "the pending failure is a notice naming the room: {notices:?}"
    );

    // The new message was really saved, so clearing the mark delivers it,
    // while the mail from before the mark stays held.
    apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("ACCEPTED"),
        "the native delivery to be accepted",
    );
    let received = fake.state.received.lock().unwrap().clone();
    assert_eq!(
        received.len(),
        1,
        "only the new message is sent: {received:?}"
    );
    assert!(received[0].1.contains("new"), "{received:?}");
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
}

/// Two pending reports with one failing write: the failure is reported
/// once, the second report succeeds, and the room still accepts mail.
#[cfg(all(test, unix))]
#[test]
fn two_pending_reports_with_one_failing_are_each_accounted_for() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, _fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let mut mailroom = Mailroom::new(50);
    let state_path = root.path().join("state").join("native-outbox.json");

    // Old mail gives the first report a hold to write.
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    crate::native_outbox::fail_pending_hold_at(&state_path);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    apply_test_report(&mut panes, router.as_mut(), 0, None, true);
    // Its own result proves the two reports above were both answered.
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "later",
        None,
    )
    .unwrap();

    let notices = drain_answered(&mut router, &mut mailroom);
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| {
            let view = mailroom.summary();
            row_status(&view, "old")
                == Some("FAILED: sent for an earlier launch of this room; not delivered")
        },
        "the stale row to settle",
    );
    let view = mailroom.summary();
    assert_eq!(
        view.lines()
            .filter(|line| line.contains("[FAILED:"))
            .count(),
        1,
        "only the stale row is failed: {view}"
    );
    assert_eq!(
        row_status(&view, "old"),
        Some("FAILED: sent for an earlier launch of this room; not delivered"),
        "{view}"
    );
    assert_eq!(
        view.matches("sent for an earlier launch of this room; not delivered")
            .count(),
        1,
        "the stale reason appears once: {view}"
    );
    assert!(
        row_status(&view, "later").is_some_and(|status| !status.contains("FAILED")),
        "the later mail keeps its own result: {view}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.contains("could not be held"))
            .count(),
        1,
        "exactly one pending failure is reported: {notices:?}"
    );
    assert!(
        notices.iter().any(|notice| notice.contains("Opencode · 1")),
        "the failure names the room: {notices:?}"
    );
    assert!(
        send_native_mail(
            &mut panes,
            router.as_mut(),
            &mut mailroom,
            "Room X",
            0,
            "after",
            None
        )
        .is_ok(),
        "a reported failure must not stop the room from accepting mail"
    );
}

/// Waits until a terminal's log stops changing: every write the pane
/// accepted has reached its child by then, so a settled log is complete.
#[cfg(all(test, unix))]
fn settled_log(path: &std::path::Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = std::fs::read(path).unwrap_or_default();
    let mut settled_at = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let current = std::fs::read(path).unwrap_or_default();
        if current != previous {
            previous = current;
            settled_at = Instant::now();
        } else if !previous.is_empty() && settled_at.elapsed() >= Duration::from_millis(400) {
            return previous;
        }
        assert!(
            Instant::now() < deadline,
            "the terminal log never settled: {previous:?}"
        );
    }
}

/// The real event-loop pass, driven from a test. Reports enter as the
/// doorbell events production receives, so the `SessionReport` arm itself
/// runs for every case a test pushes.
#[cfg(all(test, unix))]
pub(crate) struct ReportDispatch {
    doorbell: ReportEvents,
    panes: Vec<Pane>,
    pub(crate) router: Option<NativeRouter>,
    pub(crate) mailroom: Mailroom,
    gates: Vec<DeliveryGate>,
    last_output: Vec<Option<Instant>>,
    spawned_at: Vec<Instant>,
    pulses: Vec<Option<PulseSample>>,
    details: Vec<RoomDetail>,
    pending: std::collections::VecDeque<(u64, usize)>,
    submits: std::collections::VecDeque<(Instant, usize, bool)>,
    fuse: DeliveryFuse,
    paused: bool,
    notice: Notices,
    input_ready_out: Vec<bool>,
    codex: Vec<Option<pane::CodexMint>>,
    sizes: Vec<PtySize>,
    environments: Vec<pane::GuestEnvironment>,
}

/// Queues doorbell events for [`ReportDispatch`] to drain exactly like
/// the production loop drains them.
#[cfg(all(test, unix))]
struct ReportEvents {
    events: std::collections::VecDeque<DoorbellEvent>,
}

#[cfg(all(test, unix))]
impl DoorbellSource for ReportEvents {
    fn try_recv(&mut self) -> Result<DoorbellEvent, std::sync::mpsc::TryRecvError> {
        self.events
            .pop_front()
            .ok_or(std::sync::mpsc::TryRecvError::Empty)
    }

    fn path(&self) -> &std::path::Path {
        std::path::Path::new("test-doorbell.sock")
    }
}

#[cfg(all(test, unix))]
impl ReportDispatch {
    pub(crate) fn new(panes: Vec<Pane>, router: Option<NativeRouter>) -> Self {
        let now = Instant::now();
        let rooms = panes.len();
        Self {
            doorbell: ReportEvents {
                events: std::collections::VecDeque::new(),
            },
            gates: (0..rooms).map(|_| DeliveryGate::new(false)).collect(),
            last_output: vec![None; rooms],
            spawned_at: vec![now; rooms],
            pulses: vec![None; rooms],
            details: (0..rooms).map(|_| RoomDetail::default()).collect(),
            pending: std::collections::VecDeque::new(),
            submits: std::collections::VecDeque::new(),
            fuse: DeliveryFuse::new(0),
            paused: false,
            notice: Notices::default(),
            input_ready_out: Vec::new(),
            codex: (0..rooms).map(|_| None).collect(),
            sizes: (0..rooms)
                .map(|_| PtySize {
                    rows: 6,
                    cols: 60,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .collect(),
            environments: (0..rooms)
                .map(|_| pane::GuestEnvironment::new([]))
                .collect(),
            mailroom: Mailroom::new(50),
            panes,
            router,
        }
    }

    /// Queues one session report exactly as the doorbell hands it over.
    pub(crate) fn push_report(&mut self, from: usize, session: Option<&str>, pending: bool) {
        let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
        self.doorbell.events.push_back(DoorbellEvent::SessionReport(
            crate::doorbell::DoorbellSessionReport {
                from,
                session: session.map(str::to_owned),
                pending,
                reply,
            },
        ));
    }

    /// Queues one doorbell message exactly as the doorbell hands it over.
    fn push_message(&mut self, from: usize, to: usize, body: &str) {
        let (mut envelope, _answers) = crate::doorbell::DoorbellEnvelope::for_test();
        envelope.from = from;
        envelope.to = to;
        envelope.body = body.to_owned();
        self.doorbell
            .events
            .push_back(DoorbellEvent::Message(envelope));
    }

    /// Runs one event-loop pass and returns the notice the dispatch left.
    pub(crate) fn dispatch(&mut self) -> String {
        let rooms = self.panes.len();
        let now = Instant::now();
        run_rooms_iteration(
            &mut self.doorbell,
            None::<&mut Terminal<CrosstermBackend<io::Stdout>>>,
            &mut self.panes,
            &mut self.router,
            &mut self.mailroom,
            &mut self.gates,
            &mut self.last_output,
            &mut self.spawned_at,
            &mut self.pulses,
            &mut self.details,
            &mut self.pending,
            &mut self.submits,
            &mut self.fuse,
            &mut self.paused,
            &mut self.notice,
            &mut self.input_ready_out,
            &mut self.codex,
            &self.sizes,
            &self.environments,
            &[],
            100,
            rooms,
            now,
        )
        .unwrap();
        std::mem::take(&mut self.notice).ordered_lines().join("; ")
    }

    /// Runs one event-loop pass like [`ReportDispatch::dispatch`], but
    /// keeps the notices so a test can check each one's severity.
    pub(crate) fn dispatch_keep(&mut self) {
        let rooms = self.panes.len();
        let now = Instant::now();
        run_rooms_iteration(
            &mut self.doorbell,
            None::<&mut Terminal<CrosstermBackend<io::Stdout>>>,
            &mut self.panes,
            &mut self.router,
            &mut self.mailroom,
            &mut self.gates,
            &mut self.last_output,
            &mut self.spawned_at,
            &mut self.pulses,
            &mut self.details,
            &mut self.pending,
            &mut self.submits,
            &mut self.fuse,
            &mut self.paused,
            &mut self.notice,
            &mut self.input_ready_out,
            &mut self.codex,
            &self.sizes,
            &self.environments,
            &[],
            100,
            rooms,
            now,
        )
        .unwrap();
    }

    /// Pauses delivery, as the production pause does: mail is saved but
    /// not offered.
    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Sends one envelope to a room through the production native path.
    pub(crate) fn send_mail(&mut self, to: usize, body: &str) -> Result<(), String> {
        crate::app::send_native_mail(
            &mut self.panes,
            self.router.as_mut(),
            &mut self.mailroom,
            "Sender",
            to,
            body,
            None,
        )
    }

    /// Queues one control envelope as the doorbell delivers it.
    pub(crate) fn push_control(&mut self, to: usize, action: crate::doorbell::ControlAction) {
        let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
        self.doorbell
            .events
            .push_back(crate::doorbell::DoorbellEvent::Control(
                crate::doorbell::DoorbellControl {
                    from: 0,
                    to,
                    action,
                    reply,
                },
            ));
    }

    /// The arguments this room's terminal was started with.
    pub(crate) fn tui_arguments(&self, index: usize) -> Option<Vec<std::ffi::OsString>> {
        self.panes[index].native_arguments()
    }

    /// Whether the room's delivery gate is the fresh one.
    pub(crate) fn gate_ready(&self, index: usize) -> bool {
        self.gates[index] == DeliveryGate::Ready
    }

    /// The room's last observed pulse state.
    pub(crate) fn pulse_state(&self, index: usize) -> Option<PulseState> {
        self.pulses[index].as_ref().map(|pulse| pulse.state)
    }

    /// Relaunches one room through the production restart path, so a
    /// lifecycle test can move a room into a new generation.
    pub(crate) fn relaunch_claude(
        &mut self,
        index: usize,
        action: pane::ClaudeRelaunch,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let size = self.sizes[index];
        relaunch_claude_room(
            action,
            index,
            size,
            &mut self.panes,
            self.router.as_mut(),
            &mut self.gates,
            &mut self.last_output,
            &mut self.spawned_at,
            &mut self.pulses,
            &self.environments,
            Instant::now(),
        )
    }
}

/// A Claude room's own report is the only other source of its session: a
/// valid id rebinds under a new generation and is saved as reported; a
/// non-UUID is ignored.
#[cfg(all(test, unix))]
#[test]
fn a_claude_session_report_rebinds_and_a_non_uuid_is_ignored() {
    const FIRST: &str = "6a1d2f74-1f52-4e0f-9d61-0a1b2c3d4e5f";
    const SECOND: &str = "7b2e3a85-2a63-4f10-8e72-1b2c3d4e5f60";
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let program = report_test_stub(root.path(), "claude", &root.path().join("calls"));
    let spec = report_test_spec(&program, cwd.path(), "Claude · 1", true);
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut panes = vec![
        spawn_room(
            &mut LivePanes,
            spec,
            size,
            pane::GuestEnvironment::new([]),
            None,
            None,
            None,
        )
        .unwrap(),
    ];
    let mut router = NativeRouter::start(root.path().join("outbox.json"), Vec::new()).unwrap();
    pane::bind_claude_room(&mut router, &mut panes, 0, FIRST).unwrap();
    let old = router.binding_for(0).unwrap();
    let mut rig = ReportDispatch::new(panes, Some(router));

    let shown = {
        rig.push_report(0, Some("not-a-uuid"), false);
        rig.dispatch()
    };
    assert!(shown.contains("not a Claude session id"), "{shown}");
    assert_eq!(
        rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
        FIRST
    );

    rig.push_report(0, Some(SECOND), false);
    rig.dispatch();
    let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(binding.session, SECOND);
    assert!(binding.generation > old.generation);
    assert_eq!(
        crate::pane::session_state::lookup_reported("claude", cwd.path(), "Claude · 1").as_deref(),
        Some(SECOND)
    );
}

/// A session report that arrives before the mod registers keeps the
/// launch's generation: the startup intro saved against the unreported
/// placeholder is then delivered to the reported session, exactly once.
#[cfg(all(test, unix))]
#[test]
fn a_startup_intro_reaches_the_session_reported_before_the_mod_registers() {
    const REPORTED: &str = "8c3f4b96-3b74-4021-9f83-2c3d4e5f6071";
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let program = report_test_stub(root.path(), "claude", &root.path().join("calls"));
    let spec = report_test_spec(&program, cwd.path(), "Claude · 1", true);
    let panes = vec![report_test_pane(spec.clone(), None, None)];
    let mut router = NativeRouter::start(root.path().join("outbox.json"), Vec::new()).unwrap();
    // The production fresh-launch binding: a per-room bootstrap inbox and
    // the unreported placeholder for this launch.
    let mut starts = vec![Some(pane::ClaudeStart::Fresh)];
    pane::bind_claude_rooms(&mut router, std::slice::from_ref(&spec), &mut starts).unwrap();
    let launch = router.binding_for(0).unwrap();
    assert_eq!(
        launch.session,
        crate::native_session::unreported_session("1", launch.generation)
    );
    let connection: serde_json::Value = serde_json::from_slice(
        &std::fs::read(crate::claude_inbox::bootstrap_file(cwd.path(), 1).unwrap()).unwrap(),
    )
    .unwrap();
    let (url, token) = (
        connection["url"].as_str().unwrap().to_owned(),
        connection["token"].as_str().unwrap().to_owned(),
    );
    let mut rig = ReportDispatch::new(panes, Some(router));

    // The house rules are saved while the room still has its placeholder.
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Sender", 0, "house rules", None)
        .unwrap();
    // The hook reports the real session before the mod connects.
    let shown = {
        rig.push_report(0, Some(REPORTED), false);
        rig.dispatch()
    };
    assert!(shown.contains("now follows reported session"), "{shown}");
    let reported = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(reported.session, REPORTED);
    assert_eq!(
        reported.generation, launch.generation,
        "the launch generation must survive its first report"
    );

    // The mod connects: the intro must travel to the reported session.
    let mod_ = crate::claude_inbox::FakeMod::register(&url, &token);
    let deadline = Instant::now() + Duration::from_secs(10);
    let claim = loop {
        rig.dispatch();
        if let Some(claim) = mod_.next_claim() {
            break claim;
        }
        assert!(Instant::now() < deadline, "the intro was never offered");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(claim.text.ends_with("house rules"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig.mailroom.summary().contains("ACCEPTED by Claude") {
        rig.dispatch();
        assert!(Instant::now() < deadline, "{}", rig.mailroom.summary());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(mod_.next_claim().is_none(), "the intro was offered twice");
}

#[cfg(all(test, unix))]
#[test]
fn session_reports_never_touch_a_terminal() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    // Room 0 is a bound native OpenCode room; room 1 is an ordinary
    // Claude room with a live terminal of its own. Both record every byte
    // written to them, so any terminal write is caught.
    let native_log = root.path().join("native.pty");
    let raw_log = root.path().join("raw.pty");
    let native_program = report_test_stub(root.path(), "opencode", &native_log);
    let raw_program = report_test_stub(root.path(), "claude", &raw_log);
    let fake = {
        use crate::native_launch::tests::fake_room;
        fake_room(root.path(), "server", "v2.2.5")
    };
    let mut specs = vec![
        report_test_spec(&native_program, cwd.path(), "Opencode · 1", true),
        report_test_spec(&raw_program, cwd.path(), "Claude · 2", false),
    ];
    let native = {
        use crate::native_launch;
        let _ = &fake;
        native_launch::start(native_launch::NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1.1".to_owned(),
            1,
            &[],
        ))
        .unwrap()
    };
    let server = native.server();
    let binding = crate::native_outbox::Binding {
        room: native.binding().room.clone(),
        session: "ses-old-1".to_owned(),
        endpoint: native.binding().endpoint.clone(),
        generation: 1,
    };
    let router = Some(
        NativeRouter::start(
            root.path().join("state").join("native-outbox.json"),
            vec![crate::native_route::RoomEntry {
                index: 0,
                title: "Opencode · 1".to_owned(),
                binding,
                backend: crate::native_route::Backend::OpenCode(server),
            }],
        )
        .unwrap(),
    );
    let panes = vec![
        report_test_pane(specs.remove(0), Some(native), None),
        report_test_pane(specs.remove(0), None, None),
    ];
    // Pending, same id, a rebind, an unknown room and a wrong vendor,
    // each arriving as the doorbell event production dispatches, so the
    // real SessionReport arm runs for every case below.
    let mut rig = ReportDispatch::new(panes, router);
    rig.push_report(0, None, true);
    let notice = rig.dispatch();
    assert!(notice.contains("session pending after clear"), "{notice}");
    rig.push_report(0, Some("ses-old-1"), false);
    let notice = rig.dispatch();
    assert!(notice.contains("pending cleared"), "{notice}");
    rig.push_report(0, Some("ses-reported-9"), false);
    let notice = rig.dispatch();
    assert!(
        notice.contains("now follows reported session ses-reported-9"),
        "{notice}"
    );
    rig.push_report(9, Some("ses-x"), false);
    let notice = rig.dispatch();
    assert!(notice.contains("unknown room"), "{notice}");
    rig.push_report(1, Some("ses-x"), false);
    let notice = rig.dispatch();
    assert!(
        notice.contains("is not a native OpenCode, Codex or Claude room"),
        "{notice}"
    );
    // A room's PTY hands bytes to its child only on a line boundary, so
    // flush both queues with a marker of the test's own and wait for the
    // logs to settle: every byte a report wrote has reached the child by
    // then, and each log must hold nothing but that marker.
    for pane in rig.panes.iter_mut() {
        for key in [KeyCode::Char('Z'), KeyCode::Char('Q'), KeyCode::Char('\n')] {
            pane.write_key(KeyEvent::new(key, KeyModifiers::NONE))
                .unwrap();
        }
    }
    // Wait out anything still queued for the terminals.
    let end = Instant::now() + Duration::from_secs(5);
    while rig.panes.iter().any(|pane| pane.has_unwritten_input()) {
        assert!(Instant::now() < end, "writes never drained");
        std::thread::sleep(Duration::from_millis(20));
    }
    for (name, log) in [("native", &native_log), ("raw", &raw_log)] {
        let bytes = settled_log(log);
        assert_eq!(
            bytes,
            b"ZQ\n".to_vec(),
            "session reports must never write to a terminal ({name} got {bytes:?})"
        );
    }
    // And the rebind it did perform landed on the right room.
    assert_eq!(
        rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
        "ses-reported-9"
    );
}

/// Clearing an owned OpenCode room moves the pane and the router onto a new
/// session on the same server, and resets the delivery gate.
#[cfg(all(test, unix))]
#[test]
fn clearing_an_opencode_native_room_rebinds_the_router_to_the_new_session() {
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "clear", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode · 1", true);
    // Consume the launch generation a real startup would have taken, so the
    // clear mints a strictly higher one.
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![crate::native_route::RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: crate::native_route::Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    rig.gates[0] = DeliveryGate::MessageSent;
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    let warning = relaunch_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        Instant::now(),
    )
    .unwrap();
    assert!(warning.is_none(), "{warning:?}");

    let new = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_ne!(new.session, old.session);
    assert!(new.generation > old.generation);
    assert_eq!(new.room, old.room);
    assert_eq!(new.endpoint, old.endpoint);
    // The terminal the fake server received names the new session.
    let argv = fake.dir.join("tui_argv");
    let end = Instant::now() + Duration::from_secs(5);
    let mut seen = String::new();
    while Instant::now() < end {
        seen = std::fs::read_to_string(&argv).unwrap_or_default();
        if seen.contains(&new.session) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(seen.contains(&new.session), "{seen}");
    assert!(matches!(rig.gates[0], DeliveryGate::Ready));
    assert_eq!(
        rig.pulses[0].as_ref().map(|pulse| pulse.state),
        Some(PulseState::Starting)
    );
    fake.shutdown();
}

/// A clear that cannot create a session leaves the router on the old
/// binding and reports the failure.
#[cfg(all(test, unix))]
#[test]
fn a_failed_opencode_clear_leaves_the_router_on_the_old_session() {
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "clear-fail", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode · 1", true);
    // Consume the launch generation a real startup would have taken, so the
    // clear mints a strictly higher one.
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![crate::native_route::RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: crate::native_route::Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    fake.shutdown();

    let error = relaunch_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        Instant::now(),
    )
    .unwrap_err();
    assert!(!error.to_string().is_empty());
    let kept = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(kept, old);
}

/// The control arm routes ClearContext for an owned OpenCode room to the
/// clear path instead of the native refusal.
#[cfg(all(test, unix))]
#[test]
fn plain_clear_control_reaches_the_opencode_clear_path() {
    use crate::doorbell::{DoorbellControl, DoorbellEvent};
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "control", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode · 1", true);
    // Consume the launch generation a real startup would have taken, so the
    // clear mints a strictly higher one.
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![crate::native_route::RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: crate::native_route::Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    rig.sizes = vec![size];
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
    rig.doorbell
        .events
        .push_back(DoorbellEvent::Control(DoorbellControl {
            from: 0,
            to: 0,
            action: crate::doorbell::ControlAction::ClearContext,
            reply,
        }));
    rig.dispatch_keep();
    let notice = rig
        .notice
        .entries
        .last()
        .map(|entry| entry.text.clone())
        .unwrap_or_default();
    assert!(!notice.contains("cannot be restarted"), "{notice}");
    let new = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert!(new.generation > old.generation, "{notice}");
    assert_ne!(new.session, old.session);
    fake.shutdown();
}

/// A plain room's clear still goes through the ordinary path, not the
/// OpenCode one: the arm only matches owned-server rooms.
#[cfg(all(test, unix))]
#[test]
fn clear_control_on_a_plain_room_uses_the_ordinary_path() {
    use crate::doorbell::{DoorbellControl, DoorbellEvent};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let stub = report_test_stub(root.path(), "sh", &root.path().join("calls"));
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pane = report_test_pane(
        report_test_spec(&stub, cwd.path(), "Plain · 1", false),
        None,
        None,
    );
    let mut rig = ReportDispatch::new(vec![pane], None);
    rig.sizes = vec![size];
    assert!(!rig.panes[0].opencode_native());
    let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
    rig.doorbell
        .events
        .push_back(DoorbellEvent::Control(DoorbellControl {
            from: 0,
            to: 0,
            action: crate::doorbell::ControlAction::ClearContext,
            reply,
        }));
    rig.dispatch_keep();
    let notice = rig
        .notice
        .entries
        .last()
        .map(|entry| entry.text.clone())
        .unwrap_or_default();
    assert!(notice.contains("clear context"), "{notice}");
    assert!(rig.router.is_none());
}

/// A router that cannot follow a cleared room still leaves the room's own
/// state describing the terminal that started, and says so.
#[cfg(all(test, unix))]
#[test]
fn a_clear_whose_router_cannot_follow_keeps_the_new_terminal_state() {
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "control-fail", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode · 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![crate::native_route::RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: crate::native_route::Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    // No server the router can hand to a rebind: the clear itself still
    // works, because the pane owns the server.
    rig.router
        .as_mut()
        .unwrap()
        .unbind_room(0, "test: no server".to_owned())
        .unwrap();
    // Seed state that the resets must actually change, so a missing reset
    // cannot pass by matching the values it was given.
    let stale = Instant::now() - Duration::from_secs(60);
    rig.gates[0] = DeliveryGate::new(true);
    rig.last_output[0] = Some(stale);
    rig.spawned_at[0] = stale;
    rig.pulses[0] = Some(PulseSample::now(PulseState::Working, None));
    let now = Instant::now();

    let error = relaunch_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        now,
    )
    .unwrap_err();
    let said = error.to_string();
    assert!(said.contains("clear"), "{said}");
    assert!(said.contains("held"), "{said}");
    // The terminal that started is the new one, and the room says so.
    let session = said
        .split("runs session ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the error names the new session")
        .to_owned();
    assert_ne!(session, old.session);
    let argv = fake.dir.join("tui_argv");
    let end = Instant::now() + Duration::from_secs(5);
    let mut seen = String::new();
    while Instant::now() < end {
        seen = std::fs::read_to_string(&argv).unwrap_or_default();
        if seen.contains(&session) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(seen.contains(&session), "{seen}");
    assert!(matches!(rig.gates[0], DeliveryGate::Ready));
    assert!(rig.last_output[0].is_none());
    assert_eq!(rig.spawned_at[0], now);
    assert_eq!(
        rig.pulses[0].as_ref().map(|pulse| pulse.state),
        Some(PulseState::Starting)
    );
    // The router cannot reach the new session: the room has no binding and
    // the server received nothing.
    assert!(rig.router.as_ref().unwrap().binding_for(0).is_none());
    let sent =
        rig.router
            .as_mut()
            .unwrap()
            .send(&mut rig.mailroom, "Sender", 0, "after-clear", None);
    assert!(sent.is_err(), "a room with no binding must refuse mail");
    assert!(fake.state.received.lock().unwrap().is_empty());
    fake.shutdown();
}

#[cfg(all(test, unix))]
#[test]
fn a_report_rebinds_only_the_reporting_room() {
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    // Consume the launch generation a real startup would have taken.
    let _launch_generation = crate::native_launch::GenerationAllocator::new(
        crate::pane::session_state::state_directory(),
    )
    .next()
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    // Rooms 0 and 2 keep ordinary bindings; room 1 starts on a stale id.
    let fake_a = fake_room(root.path(), "a", "v2.2.5");
    let native_a = native_launch::start(native_launch::NativeLaunch::direct(
        fake_a.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        1,
        &[],
    ))
    .unwrap();
    let fake_b = fake_room(root.path(), "b", "v2.2.5");
    let native_b = native_launch::start(native_launch::NativeLaunch::direct(
        fake_b.program.as_os_str(),
        cwd.path(),
        "2.1".to_owned(),
        1,
        &[],
    ))
    .unwrap();
    let program_a = report_test_stub(root.path(), "opencode", &root.path().join("a.calls"));
    let program_b = report_test_stub(root.path(), "opencode", &root.path().join("b.calls"));
    let program_c = report_test_stub(root.path(), "codex", &root.path().join("c.args"));
    let mut specs = vec![
        report_test_spec(&program_a, cwd.path(), "Opencode · 1", true),
        report_test_spec(&program_b, cwd.path(), "Opencode · 2", true),
        report_test_spec(&program_c, cwd.path(), "Codex · 3", true),
    ];
    let server_a = native_a.server();
    let server_b = native_b.server();
    let mut router = Some(
        NativeRouter::start(
            root.path().join("state").join("native-outbox.json"),
            vec![
                crate::native_route::RoomEntry {
                    index: 0,
                    title: "Opencode · 1".to_owned(),
                    binding: native_a.binding().clone(),
                    backend: crate::native_route::Backend::OpenCode(server_a),
                },
                crate::native_route::RoomEntry {
                    index: 1,
                    title: "Opencode · 2".to_owned(),
                    binding: crate::native_outbox::Binding {
                        room: native_b.binding().room.clone(),
                        session: "ses-old-2".to_owned(),
                        endpoint: native_b.binding().endpoint.clone(),
                        generation: 1,
                    },
                    backend: crate::native_route::Backend::OpenCode(server_b),
                },
            ],
        )
        .unwrap(),
    );
    let codex_room = {
        let stub = |thread: &str| {
            let path = root.path().join(format!("codex-{thread}"));
            crate::command::write_warmed_stub(
                &path,
                &format!(
                    "printf '%s\\n' \"$@\" >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' '{thread}'",
                    root.path().join("codex.args").display()
                ),
            );
            path
        };
        let program = stub("thread-a");
        crate::codex_native::room(
            program.as_os_str().to_owned(),
            cwd.path(),
            "thread-a".to_owned(),
            Vec::new(),
        )
    };
    let mut panes = vec![
        report_test_pane(specs.remove(0), Some(native_a), None),
        report_test_pane(specs.remove(0), Some(native_b), None),
        report_test_pane(
            specs.remove(0),
            None,
            Some(&pane::CodexMint::Ready {
                uuid: "11111111-1111-4111-8111-111111111111".to_owned(),
                reused: true,
            }),
        ),
    ];
    router
        .as_mut()
        .unwrap()
        .bind_codex(vec![crate::native_route::CodexEntry {
            index: 2,
            title: "Codex · 3".to_owned(),
            room: Some(std::sync::Arc::new(codex_room)),
            generation: 1,
            reason: None,
        }])
        .unwrap();
    let notice = apply_test_report(&mut panes, router.as_mut(), 1, Some("ses_b"), false);
    assert!(
        notice.contains("now follows reported session ses_b"),
        "{notice}"
    );
    // The reporting room rebound with a new generation; the others are
    // untouched in binding, generation and adapter.
    let room0 = router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(room0.session, "ses_a");
    assert_eq!(room0.generation, 1);
    let room1 = router.as_ref().unwrap().binding_for(1).unwrap();
    assert_eq!(room1.session, "ses_b");
    assert_eq!(room1.generation, 2);
    assert_eq!(
        router.as_ref().unwrap().binding_for(2).unwrap().session,
        "thread-a"
    );
    let mut mailroom = Mailroom::new(50);
    for index in 0..3 {
        send_native_mail(
            &mut panes,
            router.as_mut(),
            &mut mailroom,
            "Room X",
            index,
            "hi",
            None,
        )
        .unwrap();
    }
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, false, false);
        if mailroom.summary().matches("ACCEPTED").count() == 3 {
            break;
        }
        assert!(Instant::now() < end, "rooms did not stay deliverable");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(test, unix))]
#[test]
fn a_report_for_an_unknown_room_changes_nothing() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, _fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses_b");
    let notice = apply_test_report(&mut panes, router.as_mut(), 9, Some("ses-x"), false);
    assert!(notice.contains("unknown room"), "{notice}");
    assert!(router.as_ref().unwrap().binding_for(0).is_some());
}

#[cfg(all(test, unix))]
#[test]
fn a_reported_id_rebinds_the_opencode_room_and_delivers_to_it() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (mut panes, mut router, fake) =
        report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
    let mut mailroom = Mailroom::new(50);
    let old = router.as_ref().unwrap().binding_for(0).unwrap();
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "old",
        None,
    )
    .unwrap();
    let end = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, true, false);
        std::thread::sleep(Duration::from_millis(20));
    }
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
    assert!(
        notice.contains("now follows reported session ses_b"),
        "{notice}"
    );
    let binding = router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(binding.session, "ses_b");
    assert_eq!(binding.generation, 2);
    assert_eq!(
        binding.endpoint, old.endpoint,
        "the rebind stays on the same owned server"
    );
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "new",
        None,
    )
    .unwrap();
    pump_router_until(
        router.as_mut().unwrap(),
        &mut mailroom,
        |mailroom| mailroom.summary().contains("ACCEPTED"),
        "the native delivery to be accepted",
    );
    let received = fake.state.received.lock().unwrap().clone();
    assert_eq!(
        received.len(),
        1,
        "only post-report mail reaches the server"
    );
    assert!(received[0].1.contains("new"), "{received:?}");
    let view = mailroom.summary();
    assert!(
        view.contains("FAILED: sent for an earlier launch"),
        "{view}"
    );
    assert!(view.contains("ACCEPTED by OpenCode"), "{view}");
}

#[cfg(all(test, unix))]
#[test]
fn a_codex_report_rebinds_persists_and_delivers_to_the_new_thread() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    // The interactive pane and the queue CLI are two processes running
    // one stub: each gets its own file, so the pane's own redirection
    // can never truncate what the queue CLI wrote.
    let queue_args = root.path().join("codex.args");
    let program = report_test_stub(root.path(), "codex", &root.path().join("codex.log"));
    // Consume the launch generation a real startup would have taken.
    let _launch_generation = crate::native_launch::GenerationAllocator::new(
        crate::pane::session_state::state_directory(),
    )
    .next()
    .unwrap();
    let specs = vec![report_test_spec(&program, cwd.path(), "Codex · 1", true)];
    let mut router =
        pane::start_native_router(&specs, &[None]).expect("native rooms start a router");
    let mint = pane::CodexMint::Ready {
        uuid: "11111111-1111-4111-8111-111111111111".to_owned(),
        reused: true,
    };
    let mut panes = vec![report_test_pane(
        specs.into_iter().next().unwrap(),
        None,
        Some(&mint),
    )];
    router
        .as_mut()
        .unwrap()
        .bind_codex(vec![crate::native_route::CodexEntry {
            index: 0,
            title: "Codex · 1".to_owned(),
            room: Some(std::sync::Arc::new(
                panes[0].codex_room_for("11111111-1111-4111-8111-111111111111".to_owned()),
            )),
            generation: 1,
            reason: None,
        }])
        .unwrap();
    let mut mailroom = Mailroom::new(50);
    let thread = "33333333-3333-4333-8333-333333333333";
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some(thread), false);
    assert!(notice.contains("now follows reported session"), "{notice}");
    assert_eq!(
        crate::pane::session_state::lookup_reported("codex", cwd.path(), "Codex · 1").as_deref(),
        Some(thread),
        "the reported thread survives a restart"
    );
    let binding = router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(binding.session, thread);
    assert_eq!(binding.generation, 2);
    send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "hi codex",
        None,
    )
    .unwrap();
    // Acceptance and the queue CLI's own argument log come from
    // different processes, so wait for both instead of assuming one
    // implies the other.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, false, false);
        let view = mailroom.summary();
        let logged = std::fs::read_to_string(&queue_args).unwrap_or_default();
        if view.contains("ACCEPTED by Codex") && logged.contains(thread) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never delivered to the reported thread:\n{view}\nlogged: {logged}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(test, unix))]
#[test]
fn a_report_for_a_room_without_native_delivery_changes_nothing() {
    use crate::native_launch::{self, tests::fake_room};
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let native_program =
        report_test_stub(root.path(), "opencode", &root.path().join("native.calls"));
    let raw_program = report_test_stub(root.path(), "claude", &root.path().join("raw.calls"));
    let fake = fake_room(root.path(), "server", "v2.2.5");
    let mut specs = vec![
        report_test_spec(&native_program, cwd.path(), "Opencode · 1", true),
        report_test_spec(&raw_program, cwd.path(), "Claude · 2", false),
    ];
    let mut natives = vec![
        native_launch::start(native_launch::NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1.1".to_owned(),
            1,
            &[],
        ))
        .ok(),
        None,
    ];
    let mut router = pane::start_native_router(&specs, &natives).unwrap();
    let mut panes = vec![
        report_test_pane(specs.remove(0), natives.remove(0), None),
        report_test_pane(specs.remove(0), natives.remove(0), None),
    ];
    let notice = apply_test_report(&mut panes, router.as_mut(), 1, Some("ses-x"), false);
    assert!(
        notice.contains("not a native OpenCode, Codex or Claude room"),
        "{notice}"
    );
    assert!(router.as_ref().unwrap().binding_for(1).is_none());
}

#[cfg(all(test, unix))]
#[test]
fn an_unsavable_codex_report_holds_the_room_and_keeps_the_corrupt_file() {
    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let path = crate::pane::session_state::state_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ not json").unwrap();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let program = report_test_stub(root.path(), "codex", &root.path().join("calls"));
    let specs = vec![report_test_spec(&program, cwd.path(), "Codex · 1", true)];
    let mut router = pane::start_native_router(&specs, &[None]).expect("router starts");
    let mint = pane::CodexMint::Ready {
        uuid: "11111111-1111-4111-8111-111111111111".to_owned(),
        reused: true,
    };
    let mut panes = vec![report_test_pane(
        specs.into_iter().next().unwrap(),
        None,
        Some(&mint),
    )];
    router
        .as_mut()
        .unwrap()
        .bind_codex(vec![crate::native_route::CodexEntry {
            index: 0,
            title: "Codex · 1".to_owned(),
            room: Some(std::sync::Arc::new(
                panes[0].codex_room_for("11111111-1111-4111-8111-111111111111".to_owned()),
            )),
            generation: 1,
            reason: None,
        }])
        .unwrap();
    let thread = "33333333-3333-4333-8333-333333333333";
    let mut mailroom = Mailroom::new(50);
    let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some(thread), false);
    assert!(notice.contains(thread), "{notice}");
    assert!(notice.contains("could not be saved"), "{notice}");
    assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
    let error = send_native_mail(
        &mut panes,
        router.as_mut(),
        &mut mailroom,
        "Room X",
        0,
        "held",
        None,
    )
    .unwrap_err();
    assert!(
        error.contains(thread),
        "the hold reason names the id: {error}"
    );
}

#[test]
fn a_signal_reaches_the_loop_and_the_reap_path() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    // The signals are routed to the flag before anything is launched and the
    // reap wait follows the run on every way out, as for Ctrl+Q.
    let run_with = &production[production.find("fn run_with(").unwrap()..];
    let run_with = &run_with[..run_with.find("\n}\n").unwrap()];
    let install = run_with
        .find("shutdown::install()")
        .expect("handlers are installed");
    let run = run_with.find("run_rooms(").expect("rooms run");
    let wait = run_with
        .find("wait_for_reaps()")
        .expect("reaping is awaited");
    assert!(install < run && run < wait);
    // The loop leaves as soon as it is asked to, before doing anything else.
    let body = &production[production.find("fn run_loop").unwrap()..];
    let loop_start = body.find("    loop {\n").expect("event loop");
    let after = &body[loop_start + "    loop {\n".len()..];
    assert!(
        after
            .trim_start()
            .starts_with("if crate::shutdown::requested() {\n            break;"),
        "the first thing the loop does is check for a shutdown request"
    );
}

#[test]
fn terminal_is_taken_only_after_the_launch() {
    let order = std::cell::RefCell::new(Vec::new());
    let (launched, entered) = launch_then_enter(
        || {
            order.borrow_mut().push("launch");
            Ok(1)
        },
        || {
            order.borrow_mut().push("enter");
            Ok(2)
        },
    )
    .unwrap();
    assert_eq!((launched, entered), (1, 2));
    assert_eq!(*order.borrow(), ["launch", "enter"]);

    // A failed launch never takes the terminal.
    let order = std::cell::RefCell::new(Vec::new());
    let result = launch_then_enter(
        || -> io::Result<()> { Err(io::Error::other("no server")) },
        || {
            order.borrow_mut().push("enter");
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(order.borrow().is_empty());
}

#[test]
fn run_rooms_reaches_the_terminal_only_through_launch_then_enter() {
    let source = include_str!("../app.rs");
    let body = &source[source.find("fn run_rooms(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let launch = body
        .find("launch_then_enter(")
        .expect("launch_then_enter is used");
    let enter = body
        .find("TerminalGuard::enter")
        .expect("terminal is entered");
    assert!(launch < enter);
    assert_eq!(body.matches("TerminalGuard::enter").count(), 1);
    assert!(!body.contains("TerminalGuard::enter()"));
    assert!(body.contains("launch_native_rooms"));
}

/// The loop needs a real terminal, so its call site is checked here while
/// the restart itself is driven through the seam by the named pane tests.
#[test]
fn run_rooms_dispatches_restart_keys_through_the_seam() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    // Source-text slicing breaks on reformat or rename, so anchor on a full signature.
    let body = &production[production.find("fn run_loop").unwrap()..];
    let body = &body[..body.find("\nfn run_rooms(").unwrap()];
    assert_eq!(
        body.matches("run_rooms_key_event(").count(),
        1,
        "the loop calls the key step exactly once"
    );
    assert_eq!(
        body.matches("loop_state(").count(),
        1,
        "the loop builds the step state in one place"
    );
    let call = body.find("run_rooms_key_event(").unwrap();
    assert!(
        body[..call].contains("Event::Key(key)"),
        "the step is called from the key arm"
    );
    let step = &production[production.find("fn run_rooms_key_event(").unwrap()..];
    let step = &step[..step.find("\n}\n").unwrap()];
    assert_eq!(
        step.matches("= restart_key(").count(),
        1,
        "the step owns the single restart_key call"
    );
    assert!(
        step.contains("force_restart_requested"),
        "the step owns the restart guard"
    );
    assert!(
        !body.contains("force_restart_requested") && !body.contains("restart_key("),
        "the loop leaves the restart decision to the step"
    );
}

/// The only direct writes to a pane's terminal that the app may contain.
/// Anything else is a new automated writer that has not been reviewed
/// against native rooms: classify it, guard it in the pane, then list it.
#[test]
fn app_has_no_unreviewed_direct_terminal_writer() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    let writers = [
        ("write_bytes", 0),
        ("push_write", 0),
        ("enqueue", 0),
        // The person's own keystrokes and scroll keys.
        ("write_key", 1),
        ("forward_page_up", 1),
        ("forward_page_down", 1),
        // Automated writers; the pane refuses them for native rooms.
        ("send_whisper", 2),
        ("resend_whisper_submit", 1),
    ];
    for (method, expected) in writers {
        let calls = production.matches(&format!(".{method}(")).count();
        assert_eq!(calls, expected, "direct calls of .{method}( in app.rs");
    }
}

fn released(keys: &str) -> String {
    // Feeds one key at a time, as the event loop does, and collects what the
    // filter lets through. `\x1b` stands for the bare Esc crossterm reports
    // when a read ends on the escape byte of a sequence.
    let mut filter = TornMouseReport::default();
    let mut out = String::new();
    for character in keys.chars() {
        let code = if character == '\x1b' {
            KeyCode::Esc
        } else {
            KeyCode::Char(character)
        };
        for key in filter.filter(KeyEvent::new(code, KeyModifiers::NONE)) {
            match key.code {
                KeyCode::Esc => out.push('\x1b'),
                KeyCode::Char(character) => out.push(character),
                _ => unreachable!(),
            }
        }
    }
    out
}

#[test]
fn a_torn_wheel_report_never_reaches_the_guest() {
    // What the user saw typed into a Codex prompt.
    assert_eq!(released("\x1b[<65;176;43M"), "\x1b");
    // Back to back, as a burst delivers them.
    assert_eq!(released("\x1b[<64;20;10M\x1b[<64;20;10M"), "\x1b\x1b");
    // A release report ends with `m` rather than `M`.
    assert_eq!(released("\x1b[<0;5;5m"), "\x1b");
}

#[test]
fn typing_after_escape_survives_in_order() {
    // Nothing here is a report, so every key must come back as pressed.
    assert_eq!(released("\x1b[1] fix"), "\x1b[1] fix");
    assert_eq!(released("\x1bhello"), "\x1bhello");
    assert_eq!(released("[<65;176;43M"), "[<65;176;43M");
    assert_eq!(released("\x1b[<9x"), "\x1b[<9x");
    // A run that never terminates is released rather than held forever.
    let digits = "1".repeat(REPORT_HELD_CEILING + 2);
    assert_eq!(
        released(&format!("\x1b[<{digits}")),
        format!("\x1b[<{digits}")
    );
}

#[test]
fn only_matching_wheel_directions_are_drained() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

    let wheel = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    };

    let up = wheel(MouseEventKind::ScrollUp);
    let down = wheel(MouseEventKind::ScrollDown);
    assert_eq!(is_wheel(&up), Some(MouseEventKind::ScrollUp));
    assert_eq!(is_wheel(&down), Some(MouseEventKind::ScrollDown));

    // A burst only absorbs its own direction; the opposite direction and
    // every non-wheel event must end the drain so they are handled instead.
    assert_ne!(is_wheel(&down), is_wheel(&up));
    assert_eq!(is_wheel(&wheel(MouseEventKind::Moved)), None);
    assert_eq!(
        is_wheel(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
        None
    );
}

#[test]
fn pane_size_uses_the_bordered_inner_area_and_clamps() {
    let normal = pane_size(Rect::new(0, 0, 80, 24));
    assert_eq!((normal.cols, normal.rows), (78, 22));

    let tiny = pane_size(Rect::new(0, 0, 1, 1));
    assert_eq!((tiny.cols, tiny.rows), (1, 1));
    assert!(!pane_has_parser_viewport(Rect::new(0, 0, 3, 3)));
    assert!(!pane_has_parser_viewport(Rect::new(0, 0, 4, 3)));
    assert!(pane_has_parser_viewport(Rect::new(0, 0, 4, 4)));
}

#[test]
fn house_rules_identify_the_room_roster_and_delegation_guidance() {
    let rules = house_rules(
        1,
        "claude · 1 (model sonnet, effort high); codex · 2 (model gpt-5, effort xhigh)",
        20,
    );
    assert!(rules.contains("you are Room 1"));
    assert!(rules.contains("$CROWDED_ROOM"));
    assert!(rules.contains(
        "Room roster: claude · 1 (model sonnet, effort high); codex · 2 (model gpt-5, effort xhigh)"
    ));
    assert!(rules.contains("numeric room number shown in the roster"));
    assert!(rules.contains("\"$CROWDED_BIN\" roster"));
    assert!(rules.contains("Announce your configured model and effort in your first response"));
    assert!(rules.contains("\"$CROWDED_BIN\" send ROOM_NUMBER"));
    assert!(rules.contains("same task ID and --role result"));
    assert!(rules.contains("\"$CROWDED_BIN\" control ROOM_NUMBER"));
    assert!(rules.contains("If a delegated task is unclear, ask the originating room."));
    assert!(rules.contains(
            "You do not need to do anything now: just check the roster and stand by until a message arrives."
        ));
    // It is the last thing the room reads.
    assert!(rules.ends_with("until a message arrives."));
    assert!(!rules.contains("untrusted peer input"));
    assert!(rules.contains("pauses after 20 successful messages"));
}

#[test]
fn welcome_roster_entries_report_configured_values_or_truthful_fallbacks() {
    assert_eq!(
        roster_entry("claude · 1", Some("sonnet"), true, Some("high"), true),
        "claude · 1 (model sonnet, effort high)"
    );
    assert_eq!(
        roster_entry("codex · 2", None, true, None, true),
        "codex · 2 (model unconfigured, effort unconfigured)"
    );
    assert_eq!(
        roster_entry("opencode · 3", Some("kimi-k3"), true, None, false),
        "opencode · 3 (model kimi-k3, effort unsupported)"
    );
    assert_eq!(
        roster_entry("bash · 4", None, false, None, false),
        "bash · 4 (model unsupported, effort unsupported)"
    );
}

fn sample(state: PulseState) -> PulseSample {
    PulseSample::now(state, None)
}

fn sample_with_model(state: PulseState, model: &str) -> PulseSample {
    PulseSample::now(state, Some(model.to_owned()))
}

fn stale_sample(state: PulseState) -> PulseSample {
    PulseSample {
        state,
        model: None,
        received_at: Instant::now() - PULSE_FRESHNESS_WINDOW - Duration::from_secs(1),
    }
}

fn stale_sample_with_model(state: PulseState, model: &str) -> PulseSample {
    PulseSample {
        state,
        model: Some(model.to_owned()),
        received_at: Instant::now() - PULSE_FRESHNESS_WINDOW - Duration::from_secs(1),
    }
}

#[test]
fn effective_model_prefers_fresh_self_report_and_keeps_control_override() {
    let now = Instant::now();
    // A fresh hook self-report of model beats an unconfigured operator value.
    assert_eq!(
        effective_model(
            None,
            Some(&sample_with_model(PulseState::Working, "m1")),
            now
        ),
        Some("m1".to_owned())
    );
    // A stale self-report does not override; it is ignored.
    assert_eq!(
        effective_model(
            None,
            Some(&stale_sample_with_model(PulseState::Working, "m1")),
            now
        ),
        None
    );
    // The operator control model wins as an explicit override even against
    // a fresh self-report.
    assert_eq!(
        effective_model(
            Some("configured"),
            Some(&sample_with_model(PulseState::Working, "m1")),
            now
        ),
        Some("configured".to_owned())
    );
    // No self-report model and no configured value stays null/unconfigured.
    assert_eq!(effective_model(None, None, now), None);
    assert_eq!(
        effective_model(None, Some(&sample(PulseState::Ready)), now),
        None
    );
}

#[test]
fn roster_state_prefers_offline_pulses_and_real_readiness() {
    let now = Instant::now();
    assert_eq!(
        roster_state(
            false,
            DeliveryGate::Ready,
            true,
            Some(sample(PulseState::Ready)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Offline,
            source: PulseSource::Offline,
        }
    );
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(sample(PulseState::Ready)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Hook,
        }
    );
    // A self-reported Ready that contradicts a busy screen falls back to
    // the gate/screen inference, exactly as before.
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            false,
            Some(sample(PulseState::Ready)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Working,
            source: PulseSource::Gate,
        }
    );
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(sample(PulseState::Thinking)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Thinking,
            source: PulseSource::Hook,
        }
    );
}

#[test]
fn stale_transient_pulse_yields_to_a_demonstrably_ready_screen() {
    let now = Instant::now();
    // A hook that self-reported "thinking"/"working" long ago must not
    // override a screen the delivery gate demonstrably shows ready; the
    // resolved source names the readiness override explicitly.
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(stale_sample(PulseState::Thinking)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Readiness,
        }
    );
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(stale_sample(PulseState::Working)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Readiness,
        }
    );
    // Terminal self-reports stay authoritative even when stale.
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(stale_sample(PulseState::Error)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Error,
            source: PulseSource::Hook,
        }
    );
    // Without a demonstrably ready screen the stale transient report
    // still stands (the gate cannot confirm deliverability).
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            false,
            Some(stale_sample(PulseState::Working)),
            now
        ),
        ResolvedPulse {
            state: PulseState::Working,
            source: PulseSource::Hook,
        }
    );
}

#[test]
fn pulse_sample_marks_a_fresh_transient_self_report_as_fresh() {
    let now = Instant::now();
    assert!(!sample(PulseState::Thinking).is_stale(now));
    assert!(stale_sample(PulseState::Thinking).is_stale(now));
}

#[test]
fn detail_only_pulse_does_not_refresh_stale_sample_or_create_state() {
    let now = Instant::now();
    let stale = stale_sample(PulseState::Thinking);
    let stale_at = stale.received_at;
    let mut room_pulses = vec![Some(stale)];
    let mut room_details = vec![RoomDetail::default()];
    apply_pulse_to_room(
        &mut room_pulses,
        &mut room_details,
        0,
        PulseState::Working,
        Some("new-model".to_owned()),
        Some(vec![DetailEvent::SubAgentStarted {
            id: "a1".to_owned(),
            kind: "Task".to_owned(),
        }]),
    );
    let sample = room_pulses[0].as_ref().unwrap();
    assert_eq!(sample.state, PulseState::Thinking);
    assert_eq!(sample.received_at, stale_at);
    assert!(sample.is_stale(now));
    assert_eq!(sample.model, Some("new-model".to_owned()));
    assert_eq!(room_details[0].sub_agents.len(), 1);

    // No-prior-sample case must not synthesize a visible state.
    let mut empty_pulses: Vec<Option<PulseSample>> = vec![None];
    let mut empty_details = vec![RoomDetail::default()];
    apply_pulse_to_room(
        &mut empty_pulses,
        &mut empty_details,
        0,
        PulseState::Working,
        Some("m2".to_owned()),
        Some(vec![DetailEvent::TodoUpsert {
            id: "t1".to_owned(),
            content: "x".to_owned(),
            status: "pending".to_owned(),
        }]),
    );
    assert!(
        empty_pulses[0].is_none(),
        "detail-only pulse must not create a sample when none existed"
    );
    assert_eq!(empty_details[0].todos.len(), 1);
}

#[test]
fn native_pulse_label_follows_state_not_a_fixed_string() {
    let source = include_str!("../app.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
    // No special case that pins native rooms to a constant label.
    assert!(!production.contains("\"native\".to_owned()"));
    // Native rooms reach the shared roster_state path like any other.
    let fn_body = &production[production.find("fn pulse_label(").unwrap()..];
    let fn_body = &fn_body[..fn_body.find("\n}\n").unwrap()];
    assert!(fn_body.contains("roster_state("));
}

#[test]
fn tui_pulse_label_drops_source_and_shows_state_plus_age() {
    assert_eq!(
        resolved_label(
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            },
            None,
        ),
        "ready"
    );
    assert_eq!(
        resolved_label(
            ResolvedPulse {
                state: PulseState::Thinking,
                source: PulseSource::Hook,
            },
            Some(Duration::from_secs(8)),
        ),
        "thinking · 8s"
    );
    assert_eq!(
        resolved_label(
            ResolvedPulse {
                state: PulseState::Offline,
                source: PulseSource::Offline,
            },
            None,
        ),
        "offline"
    );
    assert_eq!(
        resolved_label(
            ResolvedPulse {
                state: PulseState::Working,
                source: PulseSource::Gate,
            },
            None,
        ),
        "working"
    );
}

#[test]
fn pulse_badges_are_compact_and_conditional() {
    assert_eq!(pulse_badges(false, false), "");
    assert_eq!(pulse_badges(true, false), "H");
    assert_eq!(pulse_badges(false, true), "S");
    assert_eq!(pulse_badges(true, true), "H S");
}

fn line_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn render_pulse_lines_separates_rooms_and_appends_total() {
    let costs = vec![
        Some((Instant::now(), "$0.100000".to_owned())),
        Some((Instant::now(), "$0.200000".to_owned())),
        Some((Instant::now(), "$0.050000".to_owned())),
    ];
    let lines = render_pulse_lines(
        &["Kiwi K2.7 code · 3", "Claude · 1", "DeepSeek · 4"],
        &[true, false, false],
        &[true, true, true],
        &["ready · 5s", "thinking · 8s", "working"],
        &costs,
        &[],
        0,
    );
    let text: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(
        text,
        vec![
            "Kiwi K2.7 code · 3  H S",
            "  ready · 5s · $0.100000",
            "",
            "Claude · 1  S",
            "  thinking · 8s · $0.200000",
            "",
            "DeepSeek · 4  S",
            "  working · $0.050000",
            "Total: $0.350000",
        ]
    );
}

#[test]
fn pulse_total_sums_only_known_costs() {
    // All captured rooms known: no K/N qualifier.
    let costs = vec![
        Some((Instant::now(), "$0.100000".to_owned())),
        Some((Instant::now(), "$0.250000".to_owned())),
        Some((Instant::now(), "$0.050000".to_owned())),
    ];
    let all = vec![true, true, true];
    assert_eq!(
        pulse_total(&costs, &all),
        Some("Total: $0.400000".to_owned())
    );

    // One unknown among captured: K/N qualifier, unknown adds nothing.
    let costs = vec![
        Some((Instant::now(), "$0.100000".to_owned())),
        Some((Instant::now(), "unknown".to_owned())),
        Some((Instant::now(), "$0.050000".to_owned())),
    ];
    assert_eq!(
        pulse_total(&costs, &all),
        Some("Total (2/3 known): $0.150000".to_owned())
    );

    // A captured room with an unknown cost counts toward N but not K,
    // while a room that never captured a session is excluded from both
    // even when its cost figure is known.
    let costs = vec![
        Some((Instant::now(), "$0.100000".to_owned())),
        Some((Instant::now(), "$0.050000".to_owned())),
        Some((Instant::now(), "unknown".to_owned())),
    ];
    let partial = vec![true, false, true];
    assert_eq!(
        pulse_total(&costs, &partial),
        Some("Total (1/2 known): $0.100000".to_owned())
    );

    // No captured sessions: line omitted entirely.
    let none = vec![false, false, false];
    assert_eq!(pulse_total(&costs, &none), None);
}
#[test]
fn quiet_threshold_grants_headroom_panes_extra_startup_grace() {
    assert_eq!(quiet_threshold(false), HOUSE_RULES_QUIET);
    assert_eq!(
        quiet_threshold(true),
        HOUSE_RULES_QUIET + HEADROOM_STARTUP_GRACE
    );
    assert!(quiet_threshold(true) > quiet_threshold(false));
}

#[test]
fn headroom_intro_waits_for_genuine_readiness_past_the_shared_ceiling() {
    // A Headroom-wrapped room still bootstrapping must not have its
    // intro forced by the generic fixed ceiling: the wrapper's own
    // startup pause is real work, so no timeout may paste early.
    let gate = DeliveryGate::new(true);
    assert!(!gate.can_send_intro(
        false,
        INTRO_READINESS_CEILING + Duration::from_secs(1),
        intro_ceiling(true),
    ));
    // Genuine readiness still sends the Headroom intro immediately.
    assert!(gate.can_send_intro(true, Duration::ZERO, intro_ceiling(true)));
    // Non-Headroom rooms keep the shared fixed ceiling fallback.
    assert!(gate.can_send_intro(false, INTRO_READINESS_CEILING, intro_ceiling(false)));
    assert_eq!(intro_ceiling(false), INTRO_READINESS_CEILING);
    assert!(intro_ceiling(true) > intro_ceiling(false));
}

#[test]
fn headroom_startup_lane_serializes_only_windows_headroom_spawns() {
    // Windows: a second Headroom room holds for the prior one's ready
    // boundary.
    assert!(headroom_lane_holds(true, true, true));
    // The first Headroom room has no prior to wait on.
    assert!(!headroom_lane_holds(true, true, false));
    // A non-Headroom room never holds, even on Windows.
    assert!(!headroom_lane_holds(true, false, true));
    // Non-Windows platforms keep startup fully concurrent.
    assert!(!headroom_lane_holds(false, true, true));
    assert!(!headroom_lane_holds(false, false, true));
}

#[test]
fn pane_areas_tile_any_number_of_rooms() {
    let areas = pane_areas(Rect::new(0, 0, 120, 40), 3);
    assert_eq!(
        areas,
        [
            Rect::new(0, 0, 60, 20),
            Rect::new(60, 0, 60, 20),
            Rect::new(0, 20, 120, 20),
        ]
    );
    assert!(pane_areas(Rect::default(), 0).is_empty());

    let (rooms, pulse, status) = content_areas(Rect::new(0, 0, 120, 40));
    assert_eq!(rooms, Rect::new(0, 0, 84, 39));
    assert_eq!(pulse, Rect::new(84, 0, 36, 39));
    assert_eq!(status, Rect::new(0, 39, 120, 1));
}

#[test]
fn pulse_entry_style_highlights_only_the_focused_room() {
    assert_eq!(pulse_entry_style(1, 1), Style::default().fg(Color::Cyan));
    assert_eq!(pulse_entry_style(0, 1), Style::default());
    assert_eq!(pulse_entry_style(2, 1), Style::default());
}

fn state_line_style(lines: &[Line], room_index: usize) -> Style {
    let offset = if room_index > 0 { room_index } else { 0 };
    let idx = room_index * 2 + offset;
    lines.get(idx + 1).map_or(Style::default(), |l| l.style)
}

#[test]
fn render_pulse_state_color_offline_red() {
    let lines = render_pulse_lines(
        &["A · 1", "B · 2"],
        &[false, false],
        &[false, false],
        &["offline", "ready"],
        &[None, None],
        &[],
        1,
    );
    assert_eq!(state_line_style(&lines, 0), Style::default().fg(Color::Red));
}

#[test]
fn render_pulse_state_color_error_red() {
    let lines = render_pulse_lines(
        &["A · 1"],
        &[false],
        &[false],
        &["error · 3s"],
        &[None],
        &[],
        1,
    );
    assert_eq!(state_line_style(&lines, 0), Style::default().fg(Color::Red));
}

#[test]
fn render_pulse_state_color_ready_green() {
    let lines = render_pulse_lines(
        &["A · 1"],
        &[false],
        &[true],
        &["ready · 2s"],
        &[None],
        &[],
        1,
    );
    assert_eq!(
        state_line_style(&lines, 0),
        Style::default().fg(Color::Green)
    );
}

#[test]
fn render_pulse_state_color_thinking_yellow() {
    let lines = render_pulse_lines(
        &["A · 1"],
        &[false],
        &[false],
        &["thinking · 5s"],
        &[None],
        &[],
        1,
    );
    assert_eq!(
        state_line_style(&lines, 0),
        Style::default().fg(Color::Yellow)
    );
}

#[test]
fn render_pulse_state_color_working_yellow() {
    let lines = render_pulse_lines(
        &["A · 1"],
        &[false],
        &[false],
        &["working"],
        &[None],
        &[],
        1,
    );
    assert_eq!(
        state_line_style(&lines, 0),
        Style::default().fg(Color::Yellow)
    );
}

#[test]
fn render_pulse_state_color_starting_yellow() {
    let lines = render_pulse_lines(
        &["A · 1"],
        &[false],
        &[false],
        &["starting · 1s"],
        &[None],
        &[],
        1,
    );
    assert_eq!(
        state_line_style(&lines, 0),
        Style::default().fg(Color::Yellow)
    );
}

#[test]
fn render_pulse_state_color_unknown_unstyled() {
    let lines = render_pulse_lines(&["A · 1"], &[false], &[false], &["bogus"], &[None], &[], 1);
    assert_eq!(state_line_style(&lines, 0), Style::default());
}

#[test]
fn render_pulse_focused_title_stays_cyan() {
    let lines = render_pulse_lines(
        &["A · 1", "B · 2"],
        &[false, false],
        &[false, false],
        &["offline", "ready"],
        &[None, None],
        &[],
        0,
    );
    assert_eq!(lines[0].style, Style::default().fg(Color::Cyan));
    assert_eq!(lines[1].style, Style::default().fg(Color::Red));
}

#[test]
fn message_hats_are_visible_but_optional() {
    assert_eq!(message_with_hat(None, None, "hello"), "hello");
    assert_eq!(
        message_with_hat(Some("parser-fix"), Some("reviewer"), "inspect this"),
        "[task: parser-fix | requested role: reviewer]\ninspect this"
    );
}

#[test]
fn delivery_fuse_trips_and_resets_at_its_limit() {
    let mut fuse = DeliveryFuse::new(2);
    fuse.record();
    assert_eq!(fuse.remaining(), 1);
    assert!(!fuse.is_tripped());
    fuse.record();
    assert!(fuse.is_tripped());
    fuse.reset();
    assert_eq!(fuse.remaining(), 2);
}

#[test]
fn delivery_fuse_zero_limit_never_trips() {
    let mut fuse = DeliveryFuse::new(0);
    for _ in 0..100 {
        fuse.record();
    }
    assert!(!fuse.is_tripped());
    assert_eq!(fuse.remaining(), 0);
}

#[test]
fn delivery_gate_observes_fast_hook_busy_cycles_without_unlocking_a_busy_screen() {
    let mut gate = DeliveryGate::new(true);
    gate.intro_sent();
    gate.observe_pulse(PulseState::Ready, true);
    assert_eq!(gate, DeliveryGate::IntroSent);
    gate.observe_pulse(PulseState::Thinking, true);
    assert_eq!(gate, DeliveryGate::IntroRunning);
    gate.observe_pulse(PulseState::Ready, false);
    assert!(!gate.can_deliver(false));
    gate.observe_pulse(PulseState::Ready, true);
    assert!(gate.can_deliver(true));
    gate.message_sent();
    gate.observe_pulse(PulseState::Working, true);
    gate.observe_pulse(PulseState::Ready, true);
    assert!(gate.can_deliver(true));
}

#[test]
fn delivery_gate_waits_for_each_tui_busy_cycle() {
    assert_eq!(DeliveryGate::new(false), DeliveryGate::Ready);

    let mut gate = DeliveryGate::new(true);
    assert!(gate.can_send_intro(true, Duration::ZERO, INTRO_READINESS_CEILING));
    assert!(!gate.can_deliver(true));

    gate.intro_sent();
    gate.observe(true);
    assert_eq!(gate, DeliveryGate::IntroSent);
    gate.observe(false);
    gate.observe(true);
    assert!(gate.can_deliver(true));

    gate.message_sent();
    assert!(!gate.can_deliver(true));
    gate.observe(false);
    gate.observe(true);
    assert!(gate.can_deliver(true));
}

#[test]
fn whisper_submit_resend_confirms_by_busy_then_idle_and_resends_stalled() {
    // Idle-after-processing: the pane went busy consuming the whisper and
    // is idle again, so the record retires without any resend.
    assert_eq!(
        resend_whisper_submit_due(true, true, true, Duration::ZERO),
        SubmitDisposition::Retire
    );
    // Ready-looking with staged text: never busy, ready the whole time,
    // past the ceiling -- must resubmit the stalled paste.
    assert_eq!(
        resend_whisper_submit_due(true, false, true, INTRO_READINESS_CEILING),
        SubmitDisposition::Resend
    );
    // Inside the window a ready-looking pane is still uncertain: wait.
    assert_eq!(
        resend_whisper_submit_due(true, false, true, Duration::from_secs(1)),
        SubmitDisposition::Wait
    );
    // Busy (consuming) inside the window: wait.
    assert_eq!(
        resend_whisper_submit_due(true, false, false, Duration::from_secs(1)),
        SubmitDisposition::Wait
    );
    // Still busy past the ceiling: consumed, retire without a resend.
    assert_eq!(
        resend_whisper_submit_due(true, true, false, INTRO_READINESS_CEILING),
        SubmitDisposition::Retire
    );
    // Shell rooms carry the submit inside the command: idle means done,
    // and a long-running command retires at the ceiling.
    assert_eq!(
        resend_whisper_submit_due(false, false, true, Duration::ZERO),
        SubmitDisposition::Retire
    );
    assert_eq!(
        resend_whisper_submit_due(false, false, false, Duration::from_secs(1)),
        SubmitDisposition::Wait
    );
    assert_eq!(
        resend_whisper_submit_due(false, false, false, INTRO_READINESS_CEILING),
        SubmitDisposition::Retire
    );
}

#[test]
fn intro_ceiling_sends_a_stuck_rooms_intro_without_a_ready_heuristic() {
    // A Codex spinner that never quiets, or an OpenCode marker that
    // never matches the live layout, both look like this: the readiness
    // heuristic reports not-ready forever. Before the ceiling, the
    // room must not receive its intro out of turn.
    let gate = DeliveryGate::new(true);
    assert!(!gate.can_send_intro(false, Duration::from_secs(14), INTRO_READINESS_CEILING));
    // Once the ceiling elapses, the shared fallback sends it anyway.
    assert!(gate.can_send_intro(false, Duration::from_secs(15), INTRO_READINESS_CEILING));
    // A room whose heuristic genuinely reports ready never has to wait.
    assert!(DeliveryGate::new(true).can_send_intro(true, Duration::ZERO, INTRO_READINESS_CEILING));
    // Ready rooms need no intro at all, ceiling or not.
    assert!(!DeliveryGate::Ready.can_send_intro(
        false,
        Duration::from_secs(999),
        INTRO_READINESS_CEILING
    ));
}

#[test]
fn roster_state_does_not_stick_on_a_resumed_rooms_stale_starting_self_report() {
    // Resume skips the intro whisper, so the resumed process's own
    // SessionStart hook can self-report "starting" with no later Stop
    // hook to self-report "ready". Once the gate independently confirms
    // deliverability, that must win over the stale self-report and be
    // sourced as a readiness override, not a fresh hook.
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::Ready,
            true,
            Some(sample(PulseState::Starting)),
            Instant::now()
        ),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Readiness,
        }
    );
    // A genuine startup (gate not yet Ready) still reports Starting.
    assert_eq!(
        roster_state(
            true,
            DeliveryGate::AwaitingIntro,
            false,
            Some(sample(PulseState::Starting)),
            Instant::now()
        ),
        ResolvedPulse {
            state: PulseState::Starting,
            source: PulseSource::Hook,
        }
    );
}

#[test]
fn resume_control_resets_pulse_so_roster_shows_ready_immediately() {
    let resume_gate = gate_after_control(true, true);
    assert_eq!(resume_gate, DeliveryGate::Ready);
    assert_eq!(
        roster_state(true, resume_gate, true, None, Instant::now()),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Gate,
        }
    );
    assert_eq!(
        roster_state(
            true,
            resume_gate,
            true,
            Some(sample(PulseState::Starting)),
            Instant::now()
        ),
        ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Readiness,
        }
    );
    let clear_gate = gate_after_control(true, false);
    assert_eq!(clear_gate, DeliveryGate::AwaitingIntro);
    assert_eq!(
        roster_state(
            true,
            clear_gate,
            true,
            Some(sample(PulseState::Starting)),
            Instant::now()
        ),
        ResolvedPulse {
            state: PulseState::Starting,
            source: PulseSource::Hook,
        }
    );
    assert_eq!(
        roster_state(true, clear_gate, true, None, Instant::now()),
        ResolvedPulse {
            state: PulseState::Starting,
            source: PulseSource::Gate,
        }
    );
}

#[test]
fn resume_control_skips_intro_but_clear_and_configure_resend_it() {
    assert_eq!(gate_after_control(true, true), DeliveryGate::Ready);
    // Clear and configure never resume, so they resend the intro.
    assert_eq!(gate_after_control(true, false), DeliveryGate::AwaitingIntro);
    // Terminal panes take no intro either way, including across a resume.
    assert_eq!(gate_after_control(false, true), DeliveryGate::Ready);
    assert_eq!(gate_after_control(false, false), DeliveryGate::Ready);
}

#[test]
fn force_restart_key_is_f5_or_an_offline_ctrl_r() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
    assert!(
        force_restart_requested(f5, true),
        "F5 must restart an online pane"
    );
    assert!(
        force_restart_requested(f5, false),
        "F5 must restart an offline pane"
    );
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(
        !force_restart_requested(ctrl_r, true),
        "Ctrl+R on an online pane must still forward a literal 0x12 byte"
    );
    assert!(
        force_restart_requested(ctrl_r, false),
        "Ctrl+R on an offline pane must keep reviving it"
    );
}

/// A resume that could not be honoured leaves the room starting fresh with
/// no history on screen. Treating it as resumed would cost that room both
/// its intro and its session capture.
#[test]
fn a_resume_that_started_fresh_still_takes_the_intro() {
    assert_eq!(gate_after_control(true, false), DeliveryGate::AwaitingIntro);
}

#[test]
fn normal_mode_d_is_not_intercepted_as_detail_shortcut() {
    let source = std::fs::read_to_string("src/app.rs").unwrap();
    let detail_variant = format!("{}::{}", "InputMode", "Detail");
    assert!(
        !source.contains(&detail_variant),
        "Detail input mode must have been removed"
    );
    let view = format!("{}View", "RoomDetail");
    assert!(
        !source.contains(&view),
        "Detail view type must have been removed with the modal"
    );
    let helper = format!("{}_{}", "detail", "for_room");
    assert!(
        !source.contains(&helper),
        "on-demand helper must be gone (replaced by cached refresh)"
    );
}

#[test]
fn render_pulse_lines_renders_inline_detail_concisely() {
    let details = vec![
        RoomDetail {
            sub_agents: vec![
                crate::room_detail::SubAgent {
                    id: "a1".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                },
                crate::room_detail::SubAgent {
                    id: "a2".to_owned(),
                    kind: "Explore".to_owned(),
                    status: "completed".to_owned(),
                },
            ],
            todos: vec![crate::room_detail::TodoItem {
                id: "t1".to_owned(),
                content: "Build it".to_owned(),
                status: "pending".to_owned(),
            }],
        },
        RoomDetail::default(),
    ];
    let costs = vec![None, None];
    let lines = render_pulse_lines(
        &["Claude \u{00b7} 1", "Codex \u{00b7} 2"],
        &[false, false],
        &[true, true],
        &["ready", "working"],
        &costs,
        &details,
        0,
    );
    let text: Vec<String> = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(text.iter().any(|t| t.contains("sub-agents:")), "{text:?}");
    assert!(
        text.iter().any(|t| t.contains("Task (running)")),
        "{text:?}"
    );
    assert!(
        text.iter().any(|t| t.contains("Explore (completed)")),
        "{text:?}"
    );
    assert!(text.iter().any(|t| t.contains("todos:")), "{text:?}");
    assert!(
        text.iter().any(|t| t.contains("Build it [pending]")),
        "{text:?}"
    );
    let detail_lines = text
        .iter()
        .filter(|t| t.contains("sub-agents:") || t.contains("todos:"))
        .count();
    assert_eq!(detail_lines, 2);
}

#[test]
fn render_pulse_lines_truncates_narrow_detail() {
    let long_content = "a".repeat(50);
    let details = vec![RoomDetail {
        sub_agents: vec![
            crate::room_detail::SubAgent {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
                status: "running".to_owned(),
            },
            crate::room_detail::SubAgent {
                id: "a2".to_owned(),
                kind: "Task".to_owned(),
                status: "running".to_owned(),
            },
            crate::room_detail::SubAgent {
                id: "a3".to_owned(),
                kind: "Task".to_owned(),
                status: "running".to_owned(),
            },
            crate::room_detail::SubAgent {
                id: "a4".to_owned(),
                kind: "Task".to_owned(),
                status: "running".to_owned(),
            },
        ],
        todos: vec![crate::room_detail::TodoItem {
            id: "t1".to_owned(),
            content: long_content.clone(),
            status: "pending".to_owned(),
        }],
    }];
    let costs = vec![None];
    let lines = render_pulse_lines(
        &["Claude \u{00b7} 1"],
        &[false],
        &[true],
        &["ready"],
        &costs,
        &details,
        0,
    );
    let text: Vec<String> = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(text.iter().any(|t| t.contains("+2 more")), "{text:?}");
    assert!(text.iter().any(|t| t.contains("\u{2026}")), "{text:?}");
    assert!(
        !text.iter().any(|t| t.contains(&long_content)),
        "should be truncated"
    );
}

#[test]
fn detail_collection_is_cached_outside_render_path() {
    let details = vec![RoomDetail::default()];
    let costs = vec![None];
    let first = render_pulse_lines(
        &["Claude \u{00b7} 1"],
        &[false],
        &[true],
        &["ready"],
        &costs,
        &details,
        0,
    );
    let second = render_pulse_lines(
        &["Claude \u{00b7} 1"],
        &[false],
        &[true],
        &["ready"],
        &costs,
        &details,
        0,
    );
    assert_eq!(first.len(), second.len());
    let source = std::fs::read_to_string("src/app.rs").unwrap();
    // Render itself must be pure; collection happens outside via bounded cache.
    let render_start = source.find("fn render_pulse_lines").unwrap();
    let snippet = &source[render_start..render_start + 1500];
    assert!(
        !snippet.contains("thread::spawn"),
        "render must not spawn threads"
    );
    assert!(
        !snippet.contains("collect_detail("),
        "render must not scan artifacts"
    );
    assert!(
        source.contains("DETAIL_REFRESH"),
        "bounded cache interval must exist"
    );
    assert!(
        source.contains("detail_last_refresh"),
        "cache must be maintained outside draw"
    );
}
#[test]
fn pulse_panel_renders_concisely_at_real_width_and_excludes_completed_todos() {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        text::Text,
        widgets::{Paragraph, Wrap},
    };
    let details = vec![RoomDetail {
        sub_agents: vec![crate::room_detail::SubAgent {
            id: "a1".to_owned(),
            kind: "Task".to_owned(),
            status: "running".to_owned(),
        }],
        todos: vec![
            crate::room_detail::TodoItem {
                id: "t1".to_owned(),
                content: "Retrieve envelope + verify git t.. and resolve src..".to_owned(),
                status: "completed".to_owned(),
            },
            crate::room_detail::TodoItem {
                id: "t2".to_owned(),
                content: "Build feature".to_owned(),
                status: "pending".to_owned(),
            },
            crate::room_detail::TodoItem {
                id: "t3".to_owned(),
                content: "Verify no conflict".to_owned(),
                status: "in_progress".to_owned(),
            },
        ],
    }];
    let costs = vec![None];
    let lines = render_pulse_lines(
        &["DeepSeek \u{00b7} 4"],
        &[true],
        &[true],
        &["ready"],
        &costs,
        &details,
        0,
    );
    let backend = TestBackend::new(36, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            f.render_widget(
                Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false }),
                Rect {
                    x: 0,
                    y: 0,
                    width: 36,
                    height: 12,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..12 {
        for x in 0..36 {
            rendered.push_str(buffer.cell((x, y)).unwrap().symbol());
        }
        rendered.push('\n');
    }
    assert!(
        !rendered.contains("Retrieve envelope"),
        "completed todo should be absent, got: {rendered}"
    );
    assert!(rendered.contains("Build feature"), "{rendered}");
    assert!(rendered.contains("Verify no conflict"), "{rendered}");
    let non_empty = rendered.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(
        non_empty <= 10,
        "pulse panel should be concise at 36 width, got {non_empty} lines: {rendered}"
    );
    assert!(
        !rendered.contains("t.."),
        "should not contain heavily truncated verbose text"
    );
}

#[test]
fn background_collection_does_not_block_main_thread() {
    use std::{
        path::Path,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    let (tx, rx) = mpsc::channel::<(usize, RoomDetail)>();
    let mut last_refresh = None;
    let mut pending = false;
    let now = Instant::now();
    let start = Instant::now();
    let scheduled = super::schedule_detail_collection(
        RoomDetailIdentity {
            index: 0,
            guest: "codex".to_owned(),
            cwd: Path::new("/tmp").to_path_buf(),
            title: "Codex \u{00b7} 2".to_owned(),
        },
        DetailScheduleCtx {
            now,
            last_refresh: &mut last_refresh,
            pending: &mut pending,
            tx: tx.clone(),
        },
        |guest, _cwd, title| {
            thread::sleep(Duration::from_millis(250));
            assert_eq!(guest, "codex");
            assert_eq!(title, "Codex \u{00b7} 2");
            RoomDetail {
                sub_agents: vec![crate::room_detail::SubAgent {
                    id: "slow".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                }],
                todos: vec![],
            }
        },
    );
    assert!(scheduled, "should schedule");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "scheduler blocked for {elapsed:?}, should be <50ms even with 250ms collector"
    );
    assert!(
        rx.try_recv().is_err(),
        "should be pending while background sleeps"
    );
    // Pending must block a second immediate schedule
    let mut last2 = last_refresh;
    let mut pending2 = pending;
    let scheduled2 = super::schedule_detail_collection(
        RoomDetailIdentity {
            index: 0,
            guest: "codex".to_owned(),
            cwd: Path::new("/tmp").to_path_buf(),
            title: "Codex \u{00b7} 2".to_owned(),
        },
        DetailScheduleCtx {
            now,
            last_refresh: &mut last2,
            pending: &mut pending2,
            tx: tx.clone(),
        },
        |_, _, _| panic!("should not be called while pending"),
    );
    assert!(!scheduled2, "should not reschedule while pending");
    let (idx, detail) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("background collection did not finish");
    assert_eq!(idx, 0);
    assert_eq!(detail.sub_agents[0].id, "slow");
    // Simulate main loop clearing pending after recv
    pending = false;
    let later = now + Duration::from_secs(3);
    let scheduled3 = super::schedule_detail_collection(
        RoomDetailIdentity {
            index: 0,
            guest: "codex".to_owned(),
            cwd: Path::new("/tmp").to_path_buf(),
            title: "Codex \u{00b7} 2".to_owned(),
        },
        DetailScheduleCtx {
            now: later,
            last_refresh: &mut last_refresh,
            pending: &mut pending,
            tx,
        },
        |_, _, _| RoomDetail::default(),
    );
    assert!(
        scheduled3,
        "should reschedule after interval and pending cleared"
    );
}

#[cfg(unix)]
mod loop_behaviour {
    use super::*;
    use crate::config::RoomSpec;
    use crate::native_route::NativeRouter;
    use crate::pane::{GuestEnvironment, Pane};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use portable_pty::PtySize;
    use ratatui::{Terminal, backend::TestBackend};
    use std::{
        collections::VecDeque,
        io,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    #[allow(dead_code)]
    enum ScriptedStep {
        Event(Event),
        Idle,
    }

    struct ScriptedEvents {
        steps: VecDeque<ScriptedStep>,
    }

    impl ScriptedEvents {
        fn new(events: Vec<Event>) -> Self {
            Self {
                steps: events.into_iter().map(ScriptedStep::Event).collect(),
            }
        }

        #[allow(dead_code)]
        fn steps(steps: Vec<ScriptedStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl EventSource for ScriptedEvents {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            match self.steps.front() {
                Some(ScriptedStep::Event(_)) => Ok(true),
                Some(ScriptedStep::Idle) => {
                    self.steps.pop_front();
                    Ok(false)
                }
                None => Ok(false),
            }
        }

        fn read(&mut self) -> io::Result<Event> {
            match self.steps.pop_front() {
                Some(ScriptedStep::Event(event)) => Ok(event),
                _ => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "no scripted event",
                )),
            }
        }
    }

    struct SilentDoorbell {
        path: PathBuf,
    }

    impl SilentDoorbell {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl DoorbellSource for SilentDoorbell {
        fn try_recv(
            &mut self,
        ) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError> {
            Err(std::sync::mpsc::TryRecvError::Empty)
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn shell_spec(program: &Path, cwd: &Path, title: &str) -> RoomSpec {
        RoomSpec {
            name: "sh".to_owned(),
            vendor: "shell".to_owned(),
            title: title.to_owned(),
            program: program.as_os_str().to_owned(),
            args: Vec::new(),
            transport: crate::config::Transport::Shell,
            cwd: Some(cwd.to_path_buf()),
            variables: Vec::new(),
            allow_control: false,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
            native_delivery: false,
        }
    }

    fn pty_size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn the_production_loop_runs_to_completion_on_ctrl_q_and_cleans_up() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let program = if cfg!(windows) {
            PathBuf::from("cmd.exe")
        } else {
            PathBuf::from("/bin/sh")
        };
        let spec = shell_spec(&program, cwd.path(), "Room A");
        let pane = Pane::spawn(spec, pty_size(), GuestEnvironment::new([])).unwrap();
        let mut panes = vec![pane];
        let mut router: Option<NativeRouter> = None;
        let mut codex = vec![None];
        let environments = vec![GuestEnvironment::new([])];
        let now = Instant::now();
        let mut spawned_at = vec![now];
        let mut last_output = vec![None];
        let mut pane_sizes = vec![pty_size()];
        let mut notice = Notices::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut events = ScriptedEvents::new(vec![key(KeyCode::Char('q'), KeyModifiers::CONTROL)]);
        let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

        run_loop(
            &mut terminal,
            &mut events,
            &mut doorbell,
            &mut panes,
            &mut router,
            &mut codex,
            &environments,
            &mut spawned_at,
            &mut last_output,
            &mut pane_sizes,
            &[],
            100,
            &mut notice,
            &[false],
            1,
        )
        .unwrap();

        // The loop returned only after running cleanup on the scripted exit.
        assert!(!panes[0].is_online(), "pane must be cleaned up on exit");
    }
}

/// An owned OpenCode server exit is noticed, reaped and reported once: the
/// room goes offline, the wrapper leaves no zombie, and the outbox holds the
/// room's mail instead of pasting it anywhere.
#[cfg(all(test, unix))]
#[test]
fn opencode_server_death_marks_room_offline_holds_mail_and_reaps_wrapper() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-death", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    assert!(rig.panes[0].is_online());
    assert!(!rig.panes[0].poll_opencode_server_exit().unwrap());

    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .expect("wrapper pid is recorded");
    assert!(native_launch::tests::process_alive(&wrapper.to_string()));

    // SIGTERM the wrapper exactly by its recorded pid, then close the
    // endpoint, like the incident that killed the owned servers.
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    fake.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while native_launch::tests::process_alive(&wrapper.to_string()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = rig.panes[0].poll_opencode_server_exit();
    }

    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online(), "dead server reads offline");
    assert_eq!(
        rig.notice.entries.len(),
        1,
        "one notice, got {:?}",
        rig.notice.entries
    );
    let only = &rig.notice.entries[0];
    assert_eq!(only.severity, NoticeSeverity::Failure);
    assert!(only.text.contains("Opencode"), "{only:?}");
    assert!(only.text.contains("exited"), "{only:?}");

    // A second pass reports nothing again.
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert_eq!(rig.notice.entries.len(), 1, "no duplicate notice");

    // The wrapper is reaped: kill -0 now fails.
    assert!(
        !native_launch::tests::process_alive(&wrapper.to_string()),
        "wrapper leaves no zombie"
    );
    assert!(rig.panes[0].poll_opencode_server_exit().unwrap());

    // The router keeps its binding, so new mail is held, never pasted.
    assert!(rig.router.as_ref().unwrap().binding_for(0).is_some());
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "after-death", None)
        .unwrap();
    pump_router_until(
        rig.router.as_mut().unwrap(),
        &mut rig.mailroom,
        |mailroom| mailroom.summary().contains("HELD"),
        "held mail after server death",
    );
    assert!(
        !fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("after-death")),
        "nothing is sent to the dead server"
    );
    assert!(
        rig.mailroom.summary().contains("HELD"),
        "mail is held with a held status, not lost: {}",
        rig.mailroom.summary()
    );
}

/// Ctrl+R on a dead owned-OpenCode room starts a new server and session
/// through the launch path, rebinds under a new generation, and the held
/// mail plus the intro are each delivered exactly once.
#[cfg(all(test, unix))]
#[test]
fn ctrl_r_revives_dead_opencode_room_with_new_server_and_delivers_held_mail_once() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-ctrlr", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    // Kill only the wrapper; the fake endpoint stays up so the revived
    // server can announce on the same address the harness bakes in.
    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while native_launch::tests::process_alive(&wrapper.to_string()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = rig.panes[0].poll_opencode_server_exit();
    }
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());

    // Mail sent while offline is held for the lane.
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "held-while-offline", None)
        .unwrap();

    // Drive the real Ctrl+R key path: restart plus the house-rules intro.
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let event = crossterm::event::Event::Key(key);
    let mut pane_size = |_: usize| Ok(size);
    let mut state = loop_state(
        0,
        100,
        &mut pane_size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.codex,
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        &rig.environments,
        &mut rig.mailroom,
        &mut rig.notice,
    );
    assert!(!run_rooms_key_event(&event, &mut state).unwrap());
    assert!(rig.panes[0].is_online());
    let new = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(new.room, old.room, "lane is kept so held mail is adopted");
    assert!(new.generation > old.generation, "new generation");
    assert_ne!(new.session, old.session, "new session");
    assert!(
        rig.notice
            .ordered_lines()
            .iter()
            .any(|line| line.contains("restarted")),
        "restart notice: {:?}",
        rig.notice.ordered_lines()
    );
    assert!(
        rig.notice
            .ordered_lines()
            .iter()
            .any(|line| line.contains("reintroduced")),
        "intro notice: {:?}",
        rig.notice.ordered_lines()
    );

    let end = Instant::now() + Duration::from_secs(10);
    loop {
        rig.router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut rig.mailroom, false, false);
        {
            let received = fake.state.received.lock().unwrap();
            if received
                .iter()
                .any(|(_, text)| text.contains("held-while-offline"))
                && received
                    .iter()
                    .any(|(_, text)| text.contains("House rules"))
            {
                break;
            }
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for held mail and intro: {}",
            rig.mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let received = fake.state.received.lock().unwrap().clone();
    let held = received
        .iter()
        .filter(|(_, text)| text.contains("held-while-offline"))
        .count();
    let intro = received
        .iter()
        .filter(|(_, text)| text.contains("House rules"))
        .count();
    assert_eq!(held, 1, "held mail delivered once: {received:?}");
    assert_eq!(intro, 1, "house rules delivered exactly once: {received:?}");
    assert!(
        rig.mailroom.summary().contains("ACCEPTED"),
        "{}",
        rig.mailroom.summary()
    );
}

/// A revive whose new server cannot start leaves the room offline with its
/// reason instead of inventing a session.
#[cfg(all(test, unix))]
#[test]
fn failed_revive_leaves_dead_opencode_room_offline_with_reason() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-fail", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let old = native.binding().clone();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));

    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    fake.shutdown();
    std::thread::sleep(Duration::from_millis(200));
    let _ = rig.panes[0].poll_opencode_server_exit();
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());

    // Remove the server program so the new launch cannot start.
    std::fs::remove_file(&fake.program).unwrap();
    let error = revive_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        &rig.environments,
        Instant::now(),
    )
    .unwrap_err();
    assert!(!rig.panes[0].is_online(), "failed revive stays offline");
    assert_eq!(
        rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
        old.session,
        "binding is unchanged"
    );
    assert!(!error.to_string().is_empty(), "a reason is reported");
}

/// A live owned-OpenCode room is never revived: the call fails and the
/// server, session and generation are untouched.
#[cfg(all(test, unix))]
#[test]
fn live_opencode_room_is_never_revived() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::Instant;

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-live", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    let error = revive_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        &rig.environments,
        Instant::now(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("still online"), "{error}");
    assert!(rig.panes[0].is_online());
    let kept = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(kept.session, old.session);
    assert_eq!(kept.generation, old.generation);
    fake.shutdown();
}

/// A dead server whose pending hold cannot be marked is reported honestly:
/// the room still goes offline, but the notice says mail may follow the old
/// binding instead of claiming a hold the outbox never took.
#[cfg(all(test, unix))]
#[test]
fn dead_server_with_unmarkable_lane_says_mail_may_follow_old_binding() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::Duration;

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-nomark", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .expect("wrapper pid is recorded");
    // No binding, no hold to mark: the failure path is honest by construction.
    rig.router
        .as_mut()
        .unwrap()
        .unbind_room(0, "test holds nothing".to_owned())
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    fake.shutdown();
    std::thread::sleep(Duration::from_millis(200));
    let _ = rig.panes[0].poll_opencode_server_exit();

    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());
    assert_eq!(rig.notice.entries.len(), 1);
    let only = &rig.notice.entries[0];
    assert_eq!(only.severity, NoticeSeverity::Failure);
    assert!(only.text.contains("could not be marked"), "{only:?}");
    assert!(only.text.contains("may follow the old binding"), "{only:?}");

    // Mail to the unbound room is refused loudly, never pasted anywhere.
    let error = rig
        .router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "after-death", None)
        .unwrap_err();
    assert!(!error.is_empty());
    assert!(fake.state.received.lock().unwrap().is_empty());
}

/// A rebind that cannot adopt the revived binding leaves everything it
/// found: the pane keeps its previous binding, and the lane's mail stays
/// held instead of reaching the wrong session.
#[cfg(all(test, unix))]
#[test]
fn failed_rebind_leaves_room_offline_with_mail_held() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-rebind", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    // Kill the server and mark the room offline with a pending hold.
    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while native_launch::tests::process_alive(&wrapper.to_string()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = rig.panes[0].poll_opencode_server_exit();
    }
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());

    // Held mail for the lane, then a revived binding aimed at another
    // endpoint: the adopt must fail and change nothing bound.
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "rebind-proof-held", None)
        .unwrap();
    let other = fake_room(root.path(), "revive-rebind-other", "v2.2.5");
    let other_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let other_native = native_launch::start(native_launch::NativeLaunch::direct(
        other.program.as_os_str(),
        cwd.path(),
        "9.9".to_owned(),
        other_generation,
        &[],
    ))
    .unwrap();
    let bad = crate::native_outbox::Binding {
        room: old.room.clone(),
        session: other_native.binding().session.clone(),
        endpoint: old.endpoint.clone(),
        generation: other_generation,
    };
    let title = rig.panes[0].title().to_owned();
    let error = rig
        .router
        .as_mut()
        .unwrap()
        .rebind_revived_room_keep_pending(crate::native_route::RoomEntry {
            index: 0,
            title: title.clone(),
            binding: bad,
            backend: crate::native_route::Backend::OpenCode(other_native.server()),
        })
        .unwrap_err();
    assert!(!error.to_string().is_empty(), "a reason is reported");

    // The pane is untouched and still offline on its previous binding, the
    // lane map still points at the old server, and the candidate server was
    // never adopted.
    assert!(!rig.panes[0].is_online());
    assert_eq!(rig.router.as_ref().unwrap().binding_for(0).unwrap(), old);
    assert_eq!(
        rig.router
            .as_ref()
            .unwrap()
            .server_for(0)
            .unwrap()
            .handoff()
            .unwrap()
            .url,
        old.endpoint,
        "the lane map is unchanged"
    );

    // The candidate replacement is stopped by its recorded pid on drop.
    let other_wrapper = other_native
        .server()
        .wrapper_pid()
        .expect("candidate wrapper pid is recorded");
    drop(other_native);
    let deadline = Instant::now() + Duration::from_secs(10);
    while native_launch::tests::process_alive(&other_wrapper.to_string())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !native_launch::tests::process_alive(&other_wrapper.to_string()),
        "the discarded candidate server is stopped"
    );

    // Drain every queued operation, then read the persisted outbox: the row
    // is still HELD there, never delivered anywhere.
    let outbox_path = root.path().join("outbox.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    let persisted = loop {
        rig.router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut rig.mailroom, false, false);
        if let Some(held) = persisted_held_row(&outbox_path, "rebind-proof-held") {
            break held;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the persisted held row: {}",
            rig.mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(persisted, "the persisted row stays HELD");
    assert_eq!(
        persisted_hold_reason(&outbox_path, "rebind-proof-held").as_deref(),
        Some(crate::native_session::PENDING_HOLD),
        "the hold stays pending, never converted"
    );
    assert!(
        !fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("rebind-proof-held")),
        "nothing reached the wrong session"
    );
    assert!(
        !other
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("rebind-proof-held")),
        "nothing reached the other server either"
    );
    fake.shutdown();
    other.shutdown();
}

/// An adapter rebind failure after the lane server was swapped restores the
/// lane map too: the room keeps its previous binding on the previous server,
/// the discarded candidate is stopped, and the held mail stays HELD.
#[cfg(all(test, unix))]
#[test]
fn failed_adapter_rebind_restores_lane_server_and_holds_mail() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-adapter", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    // Kill the server and mark the room offline with a pending hold.
    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while native_launch::tests::process_alive(&wrapper.to_string()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = rig.panes[0].poll_opencode_server_exit();
    }
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());

    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "adapter-proof-held", None)
        .unwrap();

    // A valid revived binding on a new server, but the adapter rebind is
    // poisoned: the lane map restore must cover the server entry too.
    let other = fake_room(root.path(), "revive-adapter-other", "v2.2.5");
    let other_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let other_native = native_launch::start(native_launch::NativeLaunch::direct(
        other.program.as_os_str(),
        cwd.path(),
        old.room.clone(),
        other_generation,
        &[],
    ))
    .unwrap();
    let revived = other_native.binding().clone();
    assert_ne!(revived.session, old.session);
    rig.router.as_ref().unwrap().poison_switching_adapter();
    let error = rig
        .router
        .as_mut()
        .unwrap()
        .rebind_revived_room_keep_pending(crate::native_route::RoomEntry {
            index: 0,
            title: rig.panes[0].title().to_owned(),
            binding: revived,
            backend: crate::native_route::Backend::OpenCode(other_native.server()),
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("poisoned"),
        "the adapter failure is reported: {error}"
    );

    // The room is offline on its previous binding, and the lane resolves to
    // the previous server again: the restore covers the server map.
    assert!(!rig.panes[0].is_online());
    assert_eq!(rig.router.as_ref().unwrap().binding_for(0).unwrap(), old);
    assert_eq!(
        rig.router
            .as_ref()
            .unwrap()
            .server_for(0)
            .unwrap()
            .handoff()
            .unwrap()
            .url,
        old.endpoint,
        "the lane map restore covers the server entry"
    );
    assert!(rig.router.as_ref().unwrap().is_pending(0));

    // The discarded candidate is stopped by its recorded pid.
    let other_wrapper = other_native
        .server()
        .wrapper_pid()
        .expect("candidate wrapper pid is recorded");
    drop(other_native);
    let deadline = Instant::now() + Duration::from_secs(10);
    while native_launch::tests::process_alive(&other_wrapper.to_string())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !native_launch::tests::process_alive(&other_wrapper.to_string()),
        "the discarded candidate server is stopped"
    );

    // Drain every queued operation, then read the persisted outbox: the row
    // stays HELD with its pending hold, and nothing is delivered.
    let outbox_path = root.path().join("outbox.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        rig.router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut rig.mailroom, false, false);
        if let Some(reason) = persisted_hold_reason(&outbox_path, "adapter-proof-held") {
            assert_eq!(
                reason,
                crate::native_session::PENDING_HOLD,
                "the hold stays pending, never converted"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the persisted pending row: {}",
            rig.mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("adapter-proof-held")),
        "nothing is delivered after a failed adapter rebind"
    );
    assert!(
        !other
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("adapter-proof-held")),
        "nothing reached the candidate server either"
    );
    fake.shutdown();
    other.shutdown();
}
/// Reads the persisted native outbox and reports the hold reason stored on
/// the row carrying `marker`, if any.
#[cfg(all(test, unix))]
fn persisted_hold_reason(path: &std::path::Path, marker: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let messages = saved.get("messages")?.as_array()?;
    for message in messages {
        if message.get("text")?.as_str()?.contains(marker) {
            return message
                .get("status")?
                .get("Queued")?
                .get("hold")?
                .as_str()
                .map(str::to_owned);
        }
    }
    None
}

/// Reads the persisted native outbox and reports whether the row carrying
/// `marker` is stored with a held status (`Queued` with a hold reason).
#[cfg(all(test, unix))]
fn persisted_held_row(path: &std::path::Path, marker: &str) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let messages = saved.get("messages")?.as_array()?;
    for message in messages {
        let text = message.get("text")?.as_str()?;
        if !text.contains(marker) {
            continue;
        }
        let hold = message.get("status")?.get("Queued")?.get("hold")?;
        return Some(hold.is_string());
    }
    None
}

/// A clear whose terminal cannot respawn leaves the stored session alone:
/// the previous id is still trusted because nothing new started.
#[cfg(all(test, unix))]
#[test]
fn failed_clear_spawn_leaves_stored_session_unchanged() {
    use crate::native_launch::{self, tests::fake_room};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-marker-clear", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let title = spec.title.clone();
    let old_args = spec.args.clone();
    let mut pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let before = crate::pane::session_state::lookup("opencode", cwd.path(), &title);
    let old_binding = pane.opencode_binding().unwrap();

    // No program, no respawn: the session creation on the live server still
    // runs, but the failure must change nothing persisted or bound.
    std::fs::remove_file(&fake.program).unwrap();
    let error = pane
        .clear_opencode_session(size, launch_generation + 1)
        .unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(
        crate::pane::session_state::lookup("opencode", cwd.path(), &title),
        before,
        "no fresh marker without a started terminal"
    );
    assert_eq!(pane.room_spec().args, old_args);
    assert_eq!(pane.opencode_binding().unwrap(), old_binding);
}

/// A commit that fails after a successful rebind reports that the router
/// follows the new session but the room could not start: the pane is
/// untouched and offline, the hold is kept, and the mail stays HELD.
#[cfg(all(test, unix))]
#[test]
fn failed_commit_after_rebind_reports_press_ctrl_r_again_with_mail_held() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-commit", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    // Kill the server, mark the room offline with a pending hold, and hold
    // mail for the lane.
    let wrapper = rig
        .router
        .as_ref()
        .unwrap()
        .server_for(0)
        .unwrap()
        .wrapper_pid()
        .unwrap();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while native_launch::tests::process_alive(&wrapper.to_string()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = rig.panes[0].poll_opencode_server_exit();
    }
    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "commit-proof-held", None)
        .unwrap();

    // Break the marker save: load fails, so the commit fails after the
    // router already follows the new session.
    let path = crate::pane::session_state::state_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not state file").unwrap();

    let error = revive_opencode_room(
        0,
        size,
        &mut rig.panes,
        rig.router.as_mut(),
        &mut rig.gates,
        &mut rig.last_output,
        &mut rig.spawned_at,
        &mut rig.pulses,
        &rig.environments,
        Instant::now(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error}");
    assert!(error.to_string().contains("press Ctrl+R again"), "{error}");

    // The pane is untouched and offline on its previous binding, while the
    // router already follows the new session.
    assert!(!rig.panes[0].is_online());
    assert_eq!(rig.panes[0].opencode_binding().as_ref(), Some(&old));
    let new = rig.router.as_ref().unwrap().binding_for(0).unwrap();
    assert_eq!(new.room, old.room);
    assert!(new.generation > old.generation);
    assert_ne!(new.session, old.session);

    // The hold was never released, so the mail stays HELD in memory and in
    // the persisted outbox, and nothing is delivered.
    let outbox_path = root.path().join("outbox.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    let persisted = loop {
        rig.router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut rig.mailroom, false, false);
        if let Some(held) = persisted_held_row(&outbox_path, "commit-proof-held") {
            break held;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the persisted held row: {}",
            rig.mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(persisted, "the persisted row stays HELD");
    assert!(
        rig.mailroom.summary().contains("HELD"),
        "held with a held status: {}",
        rig.mailroom.summary()
    );
    assert!(
        !fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("commit-proof-held")),
        "nothing is delivered after a failed commit"
    );
    fake.shutdown();
}

/// A revive whose terminal cannot respawn leaves the stored session alone
/// and keeps the previous binding: nothing new started.
#[cfg(all(test, unix))]
#[test]
fn failed_revive_spawn_leaves_stored_session_unchanged() {
    use crate::native_launch::{self, tests::fake_room};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-marker", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let title = spec.title.clone();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let new_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let fresh = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        new_generation,
        &[],
    ))
    .unwrap();
    let old_binding = pane.opencode_binding().unwrap();

    // No program, no respawn: the failure must change nothing persisted.
    std::fs::remove_file(&fake.program).unwrap();
    let error = match pane.prepare_revived_tui(size, &fresh) {
        Ok(_) => panic!("a respawn without a program must fail"),
        Err(error) => error,
    };
    assert!(!error.to_string().is_empty());
    assert!(
        crate::pane::session_state::lookup("opencode", cwd.path(), &title).is_none(),
        "no fresh marker without a started terminal"
    );
    assert_eq!(pane.opencode_binding().unwrap(), old_binding);
}

/// A refused endpoint counts as death even while the wrapper child is still
/// alive: only refusal (or a reaped wrapper) may mark a room offline, never a
/// timeout or any other probe error.
#[cfg(all(test, unix))]
#[test]
fn refused_endpoint_with_live_wrapper_counts_as_death() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::Duration;

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-refused", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));

    // The wrapper lives on; only the endpoint is gone.
    fake.shutdown();
    assert!(native_launch::tests::process_alive(
        &rig.router
            .as_ref()
            .unwrap()
            .server_for(0)
            .unwrap()
            .wrapper_pid()
            .unwrap()
            .to_string()
    ));
    // The endpoint probe runs at most once a second: wait it out. The
    // wrapper is still alive here, so the death below can only come from
    // the refused endpoint, never from a reaped child.
    std::thread::sleep(Duration::from_millis(1200));

    detect_dead_opencode_servers(&mut rig.panes, rig.router.as_mut(), &mut rig.notice);
    assert!(!rig.panes[0].is_online());
    assert_eq!(rig.notice.entries.len(), 1);
    assert_eq!(rig.notice.entries[0].severity, NoticeSeverity::Failure);
    assert!(rig.notice.entries[0].text.contains("exited"));
}

/// Dropping a server stops exactly its recorded processes: the owned
/// wrapper is gone afterwards, and an unrelated process is untouched.
#[cfg(all(test, unix))]
#[test]
fn dropping_a_server_stops_only_its_recorded_processes() {
    use crate::native_launch::tests::fake_room;
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-drop", "v2.2.5");
    let server =
        crate::opencode_native::Server::start(fake.program.as_os_str(), cwd.path()).unwrap();
    let owned = server.wrapper_pid().unwrap().to_string();
    assert!(crate::native_launch::tests::process_alive(&owned));
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let other_pid = other.id().to_string();
    assert!(crate::native_launch::tests::process_alive(&other_pid));

    drop(server);
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::native_launch::tests::process_alive(&owned) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !crate::native_launch::tests::process_alive(&owned),
        "the owned server is stopped on drop"
    );
    assert!(
        crate::native_launch::tests::process_alive(&other_pid),
        "an unrelated process is never touched"
    );
    assert!(
        !crate::opencode_native::recorded_stops()
            .iter()
            .any(|pid| pid.to_string() == owned),
        "no signal is sent while the wrapper is alive"
    );

    // A reaped wrapper pid is never signalled again, even though the
    // recorded server pid equals it: the kernel may have reused it.
    let server =
        crate::opencode_native::Server::start(fake.program.as_os_str(), cwd.path()).unwrap();
    let wrapper = server.wrapper_pid().expect("wrapper pid is recorded");
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &wrapper.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::native_launch::tests::process_alive(&wrapper.to_string())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
        let _ = server.poll_exit();
    }
    assert!(server.poll_exit().unwrap(), "the wrapper exit is reaped");
    crate::opencode_native::clear_recorded_stops();
    drop(server);
    assert!(
        !crate::opencode_native::recorded_stops()
            .iter()
            .any(|pid| pid.to_string() == wrapper.to_string()),
        "a reaped wrapper pid is never signalled, even for the recorded server pid"
    );
    assert!(
        crate::native_launch::tests::process_alive(&other_pid),
        "an unrelated process is still never touched"
    );
    let _ = other.kill();
    let _ = other.wait();
    fake.shutdown();
}

// A retarget or adopt failure inside rebind_room leaves the pending hold in
// place: later mail is still held, the binding is unchanged, and nothing is
// delivered anywhere.
#[cfg(all(test, unix))]
#[test]
fn failed_rebind_room_keeps_the_pending_hold() {
    use crate::native_launch::{self, tests::fake_room};
    use crate::native_route::{Backend, NativeRouter, RoomEntry};
    use std::time::{Duration, Instant};

    let _state = crate::pane::session_state::StateRootGuard::isolated();
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let fake = fake_room(root.path(), "revive-r1", "v2.2.5");
    let size = PtySize {
        rows: 6,
        cols: 60,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = report_test_spec(&fake.program, cwd.path(), "Opencode \u{b7} 1", true);
    let launch_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let native = native_launch::start(native_launch::NativeLaunch::direct(
        fake.program.as_os_str(),
        cwd.path(),
        "1.1".to_owned(),
        launch_generation,
        &[],
    ))
    .unwrap();
    let router = NativeRouter::start(
        root.path().join("outbox.json"),
        vec![RoomEntry {
            index: 0,
            title: spec.title.clone(),
            binding: native.binding().clone(),
            backend: Backend::OpenCode(native.server()),
        }],
    )
    .unwrap();
    let pane =
        Pane::spawn_with_native(spec, size, pane::GuestEnvironment::new([]), Some(native)).unwrap();
    let mut rig = ReportDispatch::new(vec![pane], Some(router));
    let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();

    rig.router.as_mut().unwrap().mark_pending(0).unwrap();
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "r1-proof-held", None)
        .unwrap();

    // A rebinding aimed at another endpoint: the adopt fails.
    let other = fake_room(root.path(), "revive-r1-other", "v2.2.5");
    let other_generation =
        native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
            .next()
            .unwrap();
    let other_native = native_launch::start(native_launch::NativeLaunch::direct(
        other.program.as_os_str(),
        cwd.path(),
        "9.9".to_owned(),
        other_generation,
        &[],
    ))
    .unwrap();
    let bad = crate::native_outbox::Binding {
        room: old.room.clone(),
        session: other_native.binding().session.clone(),
        endpoint: old.endpoint.clone(),
        generation: other_generation,
    };
    let title = rig.panes[0].title().to_owned();
    let error = rig
        .router
        .as_mut()
        .unwrap()
        .rebind_room(crate::native_route::RoomEntry {
            index: 0,
            title,
            binding: bad,
            backend: crate::native_route::Backend::OpenCode(other_native.server()),
        })
        .unwrap_err();
    assert!(!error.to_string().is_empty(), "a reason is reported");

    // The hold is kept: the lane is still pending, the binding is unchanged,
    // later mail is still held instead of sent, and the earlier mail stays
    // HELD in the persisted outbox.
    assert!(rig.router.as_ref().unwrap().is_pending(0));
    assert_eq!(rig.router.as_ref().unwrap().binding_for(0).unwrap(), old);
    rig.router
        .as_mut()
        .unwrap()
        .send(&mut rig.mailroom, "Room X", 0, "r1-proof-after", None)
        .unwrap();
    // Drain every queued operation, then read the persisted outbox: both
    // rows keep the pending hold there, and nothing is delivered.
    let outbox_path = root.path().join("outbox.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        rig.router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut rig.mailroom, false, false);
        let first = persisted_hold_reason(&outbox_path, "r1-proof-held");
        let second = persisted_hold_reason(&outbox_path, "r1-proof-after");
        if first.as_deref() == Some(crate::native_session::PENDING_HOLD)
            && second.as_deref() == Some(crate::native_session::PENDING_HOLD)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the persisted pending rows: {}",
            rig.mailroom.summary()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        rig.mailroom
            .summary()
            .contains(crate::native_session::PENDING_HOLD),
        "later mail is still held with its pending hold: {}",
        rig.mailroom.summary()
    );
    assert!(
        !fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text.contains("r1-proof-held") || text.contains("r1-proof-after")),
        "nothing is delivered after a failed rebind"
    );
    fake.shutdown();
    other.shutdown();
}
