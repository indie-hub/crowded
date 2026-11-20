//! Deterministic lifecycle tests for native Claude delivery: the real router,
//! the durable outbox and room relaunch driven through production callers,
//! with [`FakeMod`] standing in for the room's mod. No sleeps and no timing
//! asserts: helpers pump bounded dispatch rounds with yields (deadlines only
//! guard hangs) and assert on delivered bodies, counts, generations and
//! bindings. Negative phases assert nothing is offered over bounded pumps
//! and then require a later marker message to be offered and accepted,
//! proving the loop was alive. The harness mutates no process-global state:
//! every room runs on explicit tempdir paths, and phantom sessions use
//! random ids whose files cannot exist in any config dir.

use std::path::PathBuf;

use crate::app::{ReportDispatch, report_test_pane, report_test_spec, report_test_stub};
use crate::claude_inbox::{Claim, FakeMod, bootstrap_file};
use crate::native_outbox::{is_unreported_placeholder, unreported_session};
use crate::native_route::{ClaudeEntry, NativeRouter};
use crate::pane::{self, ClaudeRelaunch, ClaudeStart, session_state::StateRootGuard};

/// One bound Claude room per title, each with its own working directory, on a
/// fresh isolated state root and a router bound to fresh placeholders. Fields
/// drop in declaration order, so the rig (router worker, room processes)
/// shuts down before the guards and tempdirs it ran on go away.
struct Fixture {
    rig: ReportDispatch,
    cwds: Vec<tempfile::TempDir>,
    titles: Vec<String>,
    /// The mod's credentials per room, read once at bind time: a report
    /// replaces the bootstrap file, but the inbox server stays the same.
    conns: Vec<(String, String)>,
    outbox_path: PathBuf,
    _root: tempfile::TempDir,
    _state: StateRootGuard,
}

impl Fixture {
    fn fresh(titles: &[&str]) -> Self {
        let _state = StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let program = report_test_stub(root.path(), "claude", &root.path().join("calls"));
        let mut cwds = Vec::new();
        let mut specs = Vec::new();
        let mut panes = Vec::new();
        for title in titles {
            let cwd = tempfile::tempdir().unwrap();
            let spec = report_test_spec(&program, cwd.path(), title, true);
            panes.push(report_test_pane(spec.clone(), None, None));
            cwds.push(cwd);
            specs.push(spec);
        }
        let outbox_path = root.path().join("outbox.json");
        let mut router = NativeRouter::start(outbox_path.clone(), Vec::new()).unwrap();
        let mut starts: Vec<Option<ClaudeStart>> =
            titles.iter().map(|_| Some(ClaudeStart::Fresh)).collect();
        let notices = pane::bind_claude_rooms(&mut router, &specs, &mut starts).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        let rig = ReportDispatch::new(panes, Some(router));
        let titles = titles.iter().map(|title| (*title).to_owned()).collect();
        let mut fx = Self {
            rig,
            cwds,
            titles,
            conns: Vec::new(),
            outbox_path,
            _root: root,
            _state,
        };
        for index in 0..fx.titles.len() {
            fx.conns.push(fx.read_bootstrap(index));
        }
        fx
    }

    fn binding(&self, index: usize) -> crate::native_outbox::Binding {
        self.rig
            .router
            .as_ref()
            .unwrap()
            .binding_for(index)
            .unwrap()
    }

    fn send(&mut self, to: usize, body: &str) {
        let rig = &mut self.rig;
        rig.router
            .as_mut()
            .unwrap()
            .send(&mut rig.mailroom, "Sender", to, body, None)
            .unwrap();
    }

    fn report(&mut self, from: usize, session: Option<&str>) -> String {
        self.rig.push_report(from, session, false);
        self.rig.dispatch()
    }

    fn report_pending(&mut self, from: usize) -> String {
        self.rig.push_report(from, None, true);
        self.rig.dispatch()
    }

    fn conn(&self, index: usize) -> (String, String) {
        self.conns[index].clone()
    }

    /// Bounded quiet pumps with positive completion evidence: nothing may be
    /// offered during the quiet pumps, then a marker sent after them must be
    /// offered and accepted, proving the loop was alive the whole time. A
    /// stale mail that could be offered would surface instead of (or before)
    /// the marker, since it is older.
    fn assert_quiet_then_marker(&mut self, mod_: &FakeMod, to: usize, marker: &str, why: &str) {
        const QUIET_PUMPS: usize = 200;
        for _ in 0..QUIET_PUMPS {
            self.rig.dispatch();
            assert!(
                mod_.next_claim().is_none(),
                "{why}: mail was offered during the quiet pumps"
            );
            std::thread::yield_now();
        }
        self.send(to, marker);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < end {
            self.rig.dispatch();
            if let Some(claim) = mod_.next_claim() {
                assert!(claim.text.ends_with(marker), "{}: {}", claim.text, why);
                assert_eq!(mod_.receipt(&claim, "started", ""), 200);
                settle(&mut self.rig, "ACCEPTED by Claude");
                return;
            }
            std::thread::yield_now();
        }
        panic!("{why}: marker was never offered");
    }

    /// Re-reads the room's bootstrap file after a relaunch or router restart
    /// writes a new one.
    fn refresh_conn(&mut self, index: usize) {
        self.conns[index] = self.read_bootstrap(index);
    }

    fn read_bootstrap(&self, index: usize) -> (String, String) {
        let raw =
            std::fs::read(bootstrap_file(self.cwds[index].path(), index + 1).unwrap()).unwrap();
        let connection: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        (
            connection["url"].as_str().unwrap().to_owned(),
            connection["token"].as_str().unwrap().to_owned(),
        )
    }

    /// Restarts the router on the same durable file with the current bindings
    /// and fresh inboxes: the saved mail must get the same verdict as before.
    fn reopen_router(&mut self) {
        let bindings: Vec<crate::native_outbox::Binding> = (0..self.titles.len())
            .map(|index| self.binding(index))
            .collect();
        for binding in &bindings {
            // Placeholder bindings rebind as-is; a reported session would need
            // its connection file, which only the hook's report path writes.
            assert!(is_unreported_placeholder(binding));
        }
        drop(self.rig.router.take());
        let path = self.outbox_path.clone();
        let mut fresh = NativeRouter::start(path, Vec::new()).unwrap();
        for (index, title) in self.titles.iter().enumerate() {
            let binding = &bindings[index];
            let inbox = fresh.claude_inbox(index).unwrap();
            inbox
                .prepare(self.cwds[index].path(), index + 1, binding.generation)
                .unwrap();
            fresh
                .bind_claude(vec![ClaudeEntry {
                    index,
                    title: title.clone(),
                    session: Some(binding.session.clone()),
                    generation: binding.generation,
                    reason: None,
                }])
                .unwrap();
        }
        self.rig.router = Some(fresh);
        for index in 0..self.titles.len() {
            self.refresh_conn(index);
        }
    }
}

/// Pumps the production loop until the mod can claim, without sleeping: the
/// loop yields so worker threads run, and the deadline only guards hangs.
fn claim_next(rig: &mut ReportDispatch, mod_: &FakeMod) -> Option<Claim> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < end {
        rig.dispatch();
        if let Some(claim) = mod_.next_claim() {
            return Some(claim);
        }
        std::thread::yield_now();
    }
    None
}

/// Pumps until the mailroom records the marker, without sleeping.
fn settle(rig: &mut ReportDispatch, marker: &str) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < end {
        rig.dispatch();
        if rig.mailroom.summary().contains(marker) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("never settled: {}", rig.mailroom.summary());
}

/// Mail held before the mod registers is delivered after it registers, under
/// the same placeholder binding and exactly once.
#[test]
fn held_mail_before_mod_registration_is_delivered_after_it_registers() {
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let before = fx.binding(0);
    assert!(is_unreported_placeholder(&before));
    fx.send(0, "hello-before-mod");
    // The schedule ran while no mod was connected: the hold is on record.
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    assert!(is_unreported_placeholder(&fx.binding(0)));
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let claim = claim_next(&mut fx.rig, &mod_).expect("held mail was never offered");
    assert!(claim.text.ends_with("hello-before-mod"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    let after = fx.binding(0);
    assert_eq!(after.session, before.session);
    assert_eq!(after.generation, before.generation);
    fx.assert_quiet_then_marker(&mod_, 0, "marker-quiet-s1", "held mail was offered twice");
}

/// The first report of a launch keeps its generation; mail saved earlier is
/// delivered to the reported session in order, exactly once. The mod keeps
/// polling with the same receiver afterwards and sees nothing twice.
#[test]
fn first_report_keeps_generation_and_delivers_once_in_order() {
    const REPORTED: &str = "8c3f4b96-3b74-4021-9f83-2c3d4e5f6071";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let launch = fx.binding(0);
    fx.send(0, "first");
    fx.send(0, "second");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    let shown = fx.report(0, Some(REPORTED));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let reported = fx.binding(0);
    assert_eq!(reported.session, REPORTED);
    assert_eq!(reported.generation, launch.generation);
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let first = claim_next(&mut fx.rig, &mod_).expect("first mail was never offered");
    assert!(first.text.ends_with("first"), "{}", first.text);
    assert_eq!(mod_.receipt(&first, "started", ""), 200);
    let second = claim_next(&mut fx.rig, &mod_).expect("second mail was never offered");
    assert!(second.text.ends_with("second"), "{}", second.text);
    assert_eq!(mod_.receipt(&second, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    // The mod survived the report with the same receiver: polling on with a
    // resumed handle shows nothing twice, then a marker must flow.
    let again = mod_.resumed();
    fx.assert_quiet_then_marker(&again, 0, "marker-quiet-s2", "mail was offered twice");
}

/// A relaunch moves the room to a new generation: old-launch mail stays held
/// and is never sent to the new session, while new mail flows. Reopening the
/// durable outbox gives the same verdict, and the mod keeps polling with the
/// same receiver afterwards and sees nothing twice.
#[test]
fn relaunch_into_a_new_generation_keeps_old_mail_stale() {
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let launch = fx.binding(0);
    fx.send(0, "old-mail");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    fx.rig.relaunch_claude(0, ClaudeRelaunch::Clear).unwrap();
    let relaunched = fx.binding(0);
    assert!(relaunched.generation > launch.generation);
    assert!(is_unreported_placeholder(&relaunched));
    fx.reopen_router();
    fx.send(0, "new-mail");
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let claim = claim_next(&mut fx.rig, &mod_).expect("new mail was never offered");
    assert!(claim.text.ends_with("new-mail"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    // The old mail is still held, visibly stale: the silence below is a
    // verdict, not an idle loop.
    settle(&mut fx.rig, "sent for an earlier launch");
    // The mod survived the relaunch with the same receiver: polling on with
    // a resumed handle shows nothing twice, then a marker must flow.
    let again = mod_.resumed();
    fx.assert_quiet_then_marker(
        &again,
        0,
        "marker-quiet-s3",
        "old-launch mail crossed generations",
    );
}

/// A session report that beats the mod registration is safe even when the
/// session file does not exist (PHANTOM-6): the hook stays the source of
/// truth, the launch keeps its generation, and a relaunch still fails safe
/// instead of resuming the phantom session.
#[test]
fn phantom_session_report_before_mod_registration_stays_safe_and_never_resumes() {
    const PHANTOM: &str = "9d4e5f60-4a85-4132-8e94-3d4e5f607182";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let launch = fx.binding(0);
    fx.send(0, "intro");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    let shown = fx.report(0, Some(PHANTOM));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let reported = fx.binding(0);
    assert_eq!(reported.session, PHANTOM);
    assert_eq!(reported.generation, launch.generation);
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let claim = claim_next(&mut fx.rig, &mod_).expect("intro was never offered");
    assert!(claim.text.ends_with("intro"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    // No session file for PHANTOM exists, so resuming must fail; clearing
    // starts a fresh placeholder under a new generation.
    assert!(fx.rig.relaunch_claude(0, ClaudeRelaunch::Resume).is_err());
    fx.rig.relaunch_claude(0, ClaudeRelaunch::Clear).unwrap();
    let clean = fx.binding(0);
    assert!(clean.generation > reported.generation);
    assert!(is_unreported_placeholder(&clean));
    // The new launch works: a fresh mod gets only new mail.
    fx.refresh_conn(0);
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    fx.send(0, "post-clear");
    let claim = claim_next(&mut fx.rig, &mod_).expect("new launch mail was never offered");
    assert!(claim.text.ends_with("post-clear"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
}

/// A pending mark adopts only mail saved after it: mail from the ended
/// session stays held as stale and is never retargeted onto the new session.
#[test]
fn pending_mark_adopts_only_pending_held_mail() {
    const FIRST: &str = "ad4e5f60-4a85-4132-8e94-3d4e5f607183";
    const SECOND: &str = "be5f6071-5b96-5243-9fa5-4e5f60718494";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let shown = fx.report(0, Some(FIRST));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let first = fx.binding(0);
    // Saved before the mark, with no mod to take it: held, not lost.
    fx.send(0, "before-the-mark");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    let shown = fx.report_pending(0);
    assert!(shown.contains("session pending after clear"), "{shown}");
    // Saved while the mark is set: held for the next session.
    fx.send(0, "while-pending");
    settle(&mut fx.rig, "HELD: session pending after clear");
    // A new session ends the old one and takes a new generation: only the
    // pending-held mail moves onto it.
    let shown = fx.report(0, Some(SECOND));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let second = fx.binding(0);
    assert_eq!(second.session, SECOND);
    assert!(second.generation > first.generation);
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let claim = claim_next(&mut fx.rig, &mod_).expect("pending-held mail was never adopted");
    assert!(claim.text.ends_with("while-pending"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    fx.assert_quiet_then_marker(&mod_, 0, "marker-quiet-s5", "stale pre-mark mail leaked");
}

/// A fresh relaunch with wiped durable state sends nothing stale and mints no
/// session: the room is an unreported placeholder until the hook reports.
#[test]
fn fresh_relaunch_with_wiped_state_sends_nothing_stale() {
    const REPORTED: &str = "bd4e5f60-4a85-4132-8e94-3d4e5f607184";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    fx.send(0, "doomed");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    let shown = fx.report(0, Some(REPORTED));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let path = fx.outbox_path.clone();
    drop(fx.rig.router.take());
    std::fs::remove_file(&path).unwrap();
    let mut fresh = NativeRouter::start(path, Vec::new()).unwrap();
    let generation = pane::next_generation().unwrap();
    let inbox = fresh.claude_inbox(0).unwrap();
    inbox.prepare(fx.cwds[0].path(), 1, generation).unwrap();
    fresh
        .bind_claude(vec![ClaudeEntry {
            index: 0,
            title: fx.titles[0].clone(),
            session: Some(unreported_session("1", generation)),
            generation,
            reason: None,
        }])
        .unwrap();
    fx.rig.router = Some(fresh);
    let clean = fx.binding(0);
    assert!(is_unreported_placeholder(&clean));
    fx.refresh_conn(0);
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    fx.assert_quiet_then_marker(&mod_, 0, "marker-quiet-s6", "wiped mail came back");
    fx.send(0, "clean-slate");
    let claim = claim_next(&mut fx.rig, &mod_).expect("new mail was never offered");
    assert!(claim.text.ends_with("clean-slate"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
}

/// Two rooms with interleaved reports keep their mail apart: no cross-room
/// or cross-generation delivery.
#[test]
fn two_rooms_with_interleaved_reports_keep_their_mail_apart() {
    const ONE: &str = "cd4e5f60-4a85-4132-8e94-3d4e5f607185";
    const TWO: &str = "dd4e5f60-4a85-4132-8e94-3d4e5f607186";
    let mut fx = Fixture::fresh(&["Claude · 1", "Claude · 2"]);
    fx.send(0, "for-room-one");
    fx.send(1, "for-room-two");
    settle(&mut fx.rig, "HELD: no Claude mod connected");
    // Interleaved: room 2 reports first.
    let shown = fx.report(1, Some(TWO));
    assert!(shown.contains("now follows reported session"), "{shown}");
    let shown = fx.report(0, Some(ONE));
    assert!(shown.contains("now follows reported session"), "{shown}");
    assert_eq!(fx.binding(0).session, ONE);
    assert_eq!(fx.binding(1).session, TWO);
    let (url0, token0) = fx.conn(0);
    let (url1, token1) = fx.conn(1);
    let mod0 = FakeMod::register(&url0, &token0);
    let mod1 = FakeMod::register(&url1, &token1);
    let claim0 = claim_next(&mut fx.rig, &mod0).expect("room 1 never got its mail");
    assert!(claim0.text.ends_with("for-room-one"), "{}", claim0.text);
    let claim1 = claim_next(&mut fx.rig, &mod1).expect("room 2 never got its mail");
    assert!(claim1.text.ends_with("for-room-two"), "{}", claim1.text);
    assert_eq!(mod0.receipt(&claim0, "started", ""), 200);
    assert_eq!(mod1.receipt(&claim1, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    fx.assert_quiet_then_marker(&mod0, 0, "marker-quiet-7a", "room 1 got room 2's mail");
    fx.assert_quiet_then_marker(&mod1, 1, "marker-quiet-7b", "room 2 got room 1's mail");
}
