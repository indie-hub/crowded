//! Deterministic lifecycle tests for native Claude delivery: the real router,
//! the durable outbox and room relaunch driven through production callers,
//! with [`FakeMod`] standing in for the room's mod. No sleeps and no timing
//! asserts: helpers pump the loop with yields under bounded deadlines (the
//! deadline only guards hangs) and assert on delivered bodies, counts,
//! generations and bindings. Negative asserts poll for a full second, about
//! ten schedule intervals, so an offer that could happen has time to happen.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::app::tests::{ReportDispatch, report_test_pane, report_test_spec, report_test_stub};
use crate::claude_inbox::{Claim, FakeMod, bootstrap_file};
use crate::native_outbox::{is_unreported_placeholder, unreported_session};
use crate::native_route::{ClaudeEntry, NativeRouter};
use crate::pane::{self, ClaudeRelaunch, ClaudeStart, session_state::StateRootGuard};

/// Saves every process-level `CROWDED_*` variable and removes it, so a stub
/// room can never touch a live room; the previous environment is restored on
/// drop. No other test mutates process-level `CROWDED_*`, so no lock is
/// needed. `CLAUDE_CONFIG_DIR` is deliberately left alone: phantom sessions
/// use random ids whose files cannot exist in any config dir.
struct EnvGuard {
    saved: Vec<(String, Option<OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(key, _)| {
                let name = key.to_string_lossy().into_owned();
                name.starts_with("CROWDED_").then_some(name)
            })
            .collect();
        let saved = names
            .into_iter()
            .map(|name| {
                let value = std::env::var_os(&name);
                // Safety: process-level `CROWDED_*` is only read by children.
                unsafe {
                    std::env::remove_var(&name);
                }
                (name, value)
            })
            .collect();
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            // Safety: restores exactly what was saved above.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// One bound Claude room per title, each with its own working directory, on a
/// fresh isolated state root and a router bound to fresh placeholders.
struct Fixture {
    _state: StateRootGuard,
    _env: EnvGuard,
    _root: tempfile::TempDir,
    cwds: Vec<tempfile::TempDir>,
    titles: Vec<String>,
    /// The mod's credentials per room, read once at bind time: a report
    /// replaces the bootstrap file, but the inbox server stays the same.
    conns: Vec<(String, String)>,
    outbox_path: PathBuf,
    rig: ReportDispatch,
}

impl Fixture {
    fn fresh(titles: &[&str]) -> Self {
        let _state = StateRootGuard::isolated();
        let _env = EnvGuard::new();
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
            _state,
            _env,
            _root: root,
            cwds,
            titles,
            conns: Vec::new(),
            outbox_path,
            rig,
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

    fn pump(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.rig.dispatch();
            std::thread::yield_now();
        }
    }

    fn conn(&self, index: usize) -> (String, String) {
        self.conns[index].clone()
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

/// Spins the loop for the whole duration, collecting anything the mod is
/// offered: a negative assert needs several schedule intervals to mean
/// anything, and spinning alone would finish in microseconds.
fn offers_within(
    rig: &mut ReportDispatch,
    mod_: &FakeMod,
    wait: std::time::Duration,
) -> Vec<String> {
    let end = std::time::Instant::now() + wait;
    let mut offered = Vec::new();
    while std::time::Instant::now() < end {
        rig.dispatch();
        while let Some(claim) = mod_.next_claim() {
            offered.push(claim.text);
        }
        std::thread::yield_now();
    }
    offered
}

/// Mail held before the mod registers is delivered after it registers, under
/// the same placeholder binding and exactly once.
#[test]
fn held_mail_before_mod_registration_is_delivered_after_it_registers() {
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let before = fx.binding(0);
    assert!(is_unreported_placeholder(&before));
    fx.send(0, "hello-before-mod");
    fx.pump(20);
    assert!(is_unreported_placeholder(&fx.binding(0)));
    let (url, token) = fx.conn(0);
    let mod_ = FakeMod::register(&url, &token);
    let claim = claim_next(&mut fx.rig, &mod_).expect("held mail was never offered");
    assert!(claim.text.ends_with("hello-before-mod"), "{}", claim.text);
    assert_eq!(mod_.receipt(&claim, "started", ""), 200);
    settle(&mut fx.rig, "ACCEPTED by Claude");
    assert!(
        offers_within(&mut fx.rig, &mod_, std::time::Duration::from_secs(1)).is_empty(),
        "held mail was offered twice"
    );
    let after = fx.binding(0);
    assert_eq!(after.session, before.session);
    assert_eq!(after.generation, before.generation);
}

/// The first report of a launch keeps its generation; mail saved earlier is
/// delivered to the reported session in order, exactly once. Re-registering
/// the same mod delivers nothing again.
#[test]
fn first_report_keeps_generation_and_delivers_once_in_order() {
    const REPORTED: &str = "8c3f4b96-3b74-4021-9f83-2c3d4e5f6071";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let launch = fx.binding(0);
    fx.send(0, "first");
    fx.send(0, "second");
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
    assert!(
        offers_within(&mut fx.rig, &mod_, std::time::Duration::from_secs(1)).is_empty(),
        "mail was offered twice"
    );
    // The mod survived the report with the same receiver: re-registering
    // while it is live is refused, and polling on shows nothing twice.
    let again = mod_.resumed();
    assert_eq!(again.register_again(), 409);
    assert!(
        offers_within(&mut fx.rig, &again, std::time::Duration::from_secs(1)).is_empty(),
        "re-registration duplicated mail"
    );
}

/// A relaunch moves the room to a new generation: old-launch mail stays held
/// and is never sent to the new session, while new mail flows. Reopening the
/// durable outbox gives the same verdict, and re-registering the same mod
/// delivers nothing again.
#[test]
fn relaunch_into_a_new_generation_keeps_old_mail_stale() {
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    let launch = fx.binding(0);
    fx.send(0, "old-mail");
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
    assert!(
        offers_within(&mut fx.rig, &mod_, std::time::Duration::from_secs(1)).is_empty(),
        "old-launch mail crossed generations"
    );
    // The mod survived the relaunch with the same receiver: re-registering
    // while it is live is refused, and polling on shows nothing twice.
    let again = mod_.resumed();
    assert_eq!(again.register_again(), 409);
    assert!(
        offers_within(&mut fx.rig, &again, std::time::Duration::from_secs(1)).is_empty(),
        "re-registration duplicated mail"
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
    fx.pump(20);
    let shown = fx.report_pending(0);
    assert!(shown.contains("session pending after clear"), "{shown}");
    // Saved while the mark is set: held for the next session.
    fx.send(0, "while-pending");
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
    assert!(
        offers_within(&mut fx.rig, &mod_, std::time::Duration::from_secs(1)).is_empty(),
        "stale pre-mark mail leaked"
    );
}

/// A fresh relaunch with wiped durable state sends nothing stale and mints no
/// session: the room is an unreported placeholder until the hook reports.
#[test]
fn fresh_relaunch_with_wiped_state_sends_nothing_stale() {
    const REPORTED: &str = "bd4e5f60-4a85-4132-8e94-3d4e5f607184";
    let mut fx = Fixture::fresh(&["Claude · 1"]);
    fx.send(0, "doomed");
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
    assert!(
        offers_within(&mut fx.rig, &mod_, std::time::Duration::from_secs(1)).is_empty(),
        "wiped mail came back"
    );
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
    assert!(
        offers_within(&mut fx.rig, &mod0, std::time::Duration::from_secs(1)).is_empty(),
        "room 1 got room 2's mail"
    );
    assert!(
        offers_within(&mut fx.rig, &mod1, std::time::Duration::from_secs(1)).is_empty(),
        "room 2 got room 1's mail"
    );
}
