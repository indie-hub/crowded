//! Durable native-delivery state. The legacy PTY mailroom is independent.
//! A stale-generation completion becomes uncertain and its receipt is discarded.
//! If directory sync fails after rename, memory tracks disk and the error names
//! the message; callers must reconcile it because durability is uncertain.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

/// Verbatim-moved session and hold rules, owned by [`crate::native_session`].
use crate::native_session::{
    CLAUDE_HOLD, PENDING_HOLD, STALE_HOLD, UNCONFIRMED_HOLD, is_retryable_hold, is_stale_hold,
    same_launch_placeholder,
};
#[cfg(test)]
use crate::native_session::{UNCERTAIN_HOLD, is_unreported_placeholder, unreported_session};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub room: String,
    pub session: String,
    pub endpoint: String,
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct Fake {
        calls: Arc<AtomicUsize>,
        result: fn() -> AdapterResult,
    }
    impl Adapter for Fake {
        fn send(&self, _: Message) -> AdapterResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }
    fn binding(room: &str, generation: u64) -> Binding {
        Binding {
            room: room.into(),
            session: "session".into(),
            endpoint: "endpoint".into(),
            generation,
        }
    }
    fn fake(calls: &Arc<AtomicUsize>, result: fn() -> AdapterResult) -> Arc<dyn Adapter> {
        Arc::new(Fake {
            calls: calls.clone(),
            result,
        })
    }
    fn accepted() -> AdapterResult {
        AdapterResult::Accepted(Some("receipt".into()))
    }

    fn pending_binding(room: &str, generation: u64) -> Binding {
        Binding {
            room: room.into(),
            session: format!("session-{generation}"),
            endpoint: "endpoint".into(),
            generation,
        }
    }

    fn hold_of(outbox: &mut Outbox, id: &str) -> Option<String> {
        outbox
            .state
            .messages
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| match &m.status {
                Status::Queued { hold } => hold.clone(),
                _ => None,
            })
    }

    #[test]
    fn mail_saved_while_its_room_is_pending_is_held_and_never_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        // The pending report lands before the mail: the mail is saved after.
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&Arc::new(AtomicUsize::new(0)), accepted),
            )
            .unwrap();
        // Command order decides, not wall-clock time: no sleep needed.
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "held mail".into())
            .unwrap()
            .id;
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "nothing may be sent while pending"
        );
        assert_eq!(hold_of(&mut outbox, &id), Some(PENDING_HOLD.to_owned()));
        // Clearing the pending mark does not release the hold on its own;
        // only adopting the mail onto a binding does.
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(hold_of(&mut outbox, &id), Some(PENDING_HOLD.to_owned()));
    }

    #[test]
    fn pending_held_mail_is_adopted_onto_the_reported_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&Arc::new(AtomicUsize::new(0)), accepted),
            )
            .unwrap();
        // Command order decides, not wall-clock time: no sleep needed.
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "held mail".into())
            .unwrap()
            .id;
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        assert_eq!(hold_of(&mut outbox, &id), Some(PENDING_HOLD.to_owned()));
        let next = pending_binding("a", 2);
        outbox.retarget("a", &next).unwrap();
        let bindings = HashMap::from([("a".to_owned(), next.clone())]);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let message = outbox.state.messages.iter().find(|m| m.id == id).unwrap();
        assert_eq!(message.binding, next);
        assert_eq!(message.status, Status::Accepted);
    }

    #[test]
    fn mail_saved_before_the_pending_report_stays_stale_and_is_never_retargeted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "old mail".into())
            .unwrap()
            .id;
        // Saved before the pending mark lands, even in the same millisecond:
        // command order decides, so the later retarget must leave it alone.
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        // Saved before the report, so it belongs to the ended session.
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
        let next = pending_binding("a", 2);
        outbox.retarget("a", &next).unwrap();
        let bindings = HashMap::from([("a".to_owned(), next)]);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "old mail is never adopted");
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
    }

    /// The pending command itself holds the mail that is already queued: with
    /// no scheduling pass in between, a later report for the same session has
    /// nothing left to release, so mail saved before the mark never reaches
    /// the adapter.
    #[test]
    fn the_pending_command_holds_already_queued_mail_with_no_schedule_between() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "old mail".into())
            .unwrap()
            .id;
        // The pending command lands on its own: the boundary classification
        // is what holds this mail, not a later schedule.
        outbox.set_pending(&["a".to_owned()]).unwrap();
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
        // The same session reports back. Its retarget only adopts mail held
        // for the pending signal, so this mail stays where it is.
        outbox.retarget("a", &binding("a", 1)).unwrap();
        outbox.set_pending(&[]).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "old mail is never sent");
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
    }

    /// A hold that cannot reach disk still blocks the mail in memory: the
    /// pending command reports the failure, and the same-session clear that
    /// follows finds nothing it may release.
    #[test]
    fn a_failed_pending_save_keeps_old_mail_held_and_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "old mail".into())
            .unwrap()
            .id;
        // Point the state at a missing parent, so the next save fails.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        outbox.path = blocked.join("outbox.json");
        assert!(
            outbox.set_pending(&["a".to_owned()]).is_err(),
            "the pending command reports the failure"
        );
        assert_eq!(
            hold_of(&mut outbox, &id),
            Some(STALE_HOLD.to_owned()),
            "the hold lands in memory even though it could not be written"
        );
        // The disk comes back; the same session reports and the mark clears.
        outbox.path = path;
        outbox.retarget("a", &binding("a", 1)).unwrap();
        outbox.set_pending(&[]).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "old mail is never sent");
        let message = outbox.state.messages.iter().find(|m| m.id == id).unwrap();
        assert_ne!(
            message.status,
            Status::Accepted,
            "old mail is never marked accepted"
        );
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
    }

    #[test]
    fn retargeting_refuses_a_binding_for_another_room() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&Arc::new(AtomicUsize::new(0)), accepted),
            )
            .unwrap();
        // Command order decides, not wall-clock time: no sleep needed.
        let id = outbox
            .enqueue("source".into(), binding("a", 1), "held mail".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&Arc::new(AtomicUsize::new(0)), accepted),
            )
            .unwrap();
        assert!(outbox.retarget("a", &pending_binding("b", 2)).is_err());
        let message = outbox.state.messages.iter().find(|m| m.id == id).unwrap();
        assert_eq!(message.binding, binding("a", 1));
        assert_eq!(hold_of(&mut outbox, &id), Some(PENDING_HOLD.to_owned()));
    }

    #[test]
    fn retargeting_onto_the_same_binding_releases_pending_held_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let bindings = HashMap::from([("a".to_owned(), binding("a", 1))]);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&Arc::new(AtomicUsize::new(0)), accepted),
            )
            .unwrap();
        // Command order decides, not wall-clock time: no sleep needed.
        outbox
            .enqueue("source".into(), binding("a", 1), "held mail".into())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        outbox.retarget("a", &binding("a", 1)).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// An adapter whose room can receive only after its mod registered.
    struct ModFake {
        ready: Arc<std::sync::atomic::AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    impl Adapter for ModFake {
        fn send(&self, _: Message) -> AdapterResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.ready.load(Ordering::SeqCst) {
                return AdapterResult::Dropped(CLAUDE_HOLD.to_owned());
            }
            accepted()
        }
        fn ready(&self, _: &str) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
    }

    /// Mail saved before a Claude mod connects is held, not lost: once the mod
    /// registers it is offered exactly once, while the room still has the
    /// launch's own unreported placeholder binding.
    #[test]
    fn startup_mail_held_for_a_missing_mod_is_delivered_once_it_registers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let startup = Binding {
            room: "a".to_owned(),
            session: unreported_session("a", 1),
            endpoint: "claude".to_owned(),
            generation: 1,
        };
        let startup_bindings = HashMap::from([("a".to_owned(), startup.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let adapter: Arc<dyn Adapter> = Arc::new(ModFake {
            ready: Arc::clone(&ready),
            calls: Arc::clone(&calls),
        });

        let id = outbox
            .enqueue("source".into(), startup, "house rules".into())
            .unwrap()
            .id;
        outbox
            .schedule(&startup_bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        wait(&mut outbox, &startup_bindings);
        assert_eq!(hold_of(&mut outbox, &id), Some(CLAUDE_HOLD.to_owned()));

        // While no mod is registered the held mail is not retried.
        outbox
            .schedule(&startup_bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The mod registers in the same launch: the intro is offered once.
        ready.store(true, Ordering::SeqCst);
        outbox
            .schedule(&startup_bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        wait(&mut outbox, &startup_bindings);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let delivered = outbox
            .state
            .messages
            .iter()
            .find(|message| message.id == id)
            .expect("the intro is kept as history");
        assert_eq!(delivered.status, Status::Accepted);
        assert_eq!(delivered.binding, startup_bindings["a"]);

        // Accepted mail is never offered again.
        outbox
            .schedule(&startup_bindings, false, false, &[], adapter)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Mail held for one session or launch is never delivered to another: a
    /// relaunch on session B must not receive session A's held mail.
    #[test]
    fn held_mail_for_an_older_session_is_never_delivered_to_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let old = binding("a", 1);
        let old_bindings = HashMap::from([("a".to_owned(), old.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let id = outbox
            .enqueue("source".into(), old.clone(), "old mail".into())
            .unwrap()
            .id;
        outbox
            .schedule(&old_bindings, false, false, &[], fake(&calls, held))
            .unwrap();
        wait(&mut outbox, &old_bindings);
        assert_eq!(hold_of(&mut outbox, &id), Some(CLAUDE_HOLD.to_owned()));

        // The room relaunches on another session; its mod is live.
        let next = pending_binding("a", 2);
        let next_bindings = HashMap::from([("a".to_owned(), next)]);
        let delivered = Arc::new(AtomicUsize::new(0));
        outbox
            .schedule(
                &next_bindings,
                false,
                false,
                &[],
                fake(&delivered, accepted),
            )
            .unwrap();
        assert_eq!(
            delivered.load(Ordering::SeqCst),
            0,
            "old mail reached the new session"
        );
        assert_eq!(hold_of(&mut outbox, &id), Some(STALE_HOLD.to_owned()));
        assert_eq!(
            outbox
                .state
                .messages
                .iter()
                .find(|message| message.id == id)
                .unwrap()
                .binding,
            old
        );
    }

    /// An adapter that records the order it was asked to send in, and can be
    /// unreachable for exactly the first readiness check.
    struct OrderedFake {
        checks: AtomicUsize,
        order: std::sync::Mutex<Vec<String>>,
    }
    impl Adapter for OrderedFake {
        fn send(&self, message: Message) -> AdapterResult {
            self.order.lock().unwrap().push(message.text);
            accepted()
        }
        fn ready(&self, _: &str) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) > 0
        }
    }

    /// While an older message is held the room is blocked: a newer message is
    /// never sent before it, even across a readiness change inside one pass.
    #[test]
    fn an_older_held_message_is_never_overtaken_by_newer_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let b = binding("a", 1);
        let bindings = HashMap::from([("a".to_owned(), b.clone())]);
        let held_calls = Arc::new(AtomicUsize::new(0));
        outbox
            .enqueue("source".into(), b.clone(), "older".into())
            .unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&held_calls, held))
            .unwrap();
        wait(&mut outbox, &bindings);
        outbox
            .enqueue("source".into(), b.clone(), "newer".into())
            .unwrap();
        let adapter = Arc::new(OrderedFake {
            checks: AtomicUsize::new(0),
            order: std::sync::Mutex::new(Vec::new()),
        });
        let as_adapter: Arc<dyn Adapter> = Arc::clone(&adapter) as Arc<dyn Adapter>;

        // The room cannot receive on the first look: nothing may go out, and
        // in particular the newer message must not overtake the older hold.
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&as_adapter))
            .unwrap();
        assert!(
            outbox
                .state
                .messages
                .iter()
                .all(|message| message.status != Status::Sending),
            "newer mail was sent while the older message was held"
        );

        // The room can receive: the older message goes first, then the newer.
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&as_adapter))
            .unwrap();
        wait(&mut outbox, &bindings);
        outbox
            .schedule(&bindings, false, false, &[], as_adapter)
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(
            *adapter.order.lock().unwrap(),
            vec!["older".to_owned(), "newer".to_owned()]
        );
    }

    fn held() -> AdapterResult {
        AdapterResult::Dropped(CLAUDE_HOLD.to_owned())
    }

    /// A message that may already have arrived is never resent, and it must
    /// not hold the room's later mail: nothing can settle it within a launch.
    #[test]
    fn newer_mail_behind_an_uncertain_message_is_still_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mut outbox = Outbox::open(&path, 4, 4, 2).unwrap();
        let b = binding("a", 1);
        let bindings = HashMap::from([("a".to_owned(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let first = outbox
            .enqueue("s".into(), b.clone(), "first".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);

        outbox.enqueue("s".into(), b, "second".into()).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(outbox.state.messages[1].status, Status::Sending);
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[1].status, Status::Accepted);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // The uncertain row survives a restart with its one attempt and is
        // never sent again.
        drop(outbox);
        let mut outbox = Outbox::open(&path, 4, 4, 2).unwrap();
        let first_row = outbox
            .state
            .messages
            .iter()
            .find(|m| m.id == first)
            .unwrap();
        assert_eq!(first_row.status, Status::Uncertain);
        assert_eq!(first_row.attempts.len(), 1);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        outbox.collect(&bindings).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn dropped() -> AdapterResult {
        AdapterResult::Dropped("stopped".into())
    }
    fn wait(outbox: &mut Outbox, bindings: &HashMap<String, Binding>) {
        let completion = outbox.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        outbox.tx.send(completion).unwrap();
        outbox.collect(bindings).unwrap();
    }

    #[test]
    fn crash_before_send_and_after_acceptance_before_receipt_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        outbox
            .enqueue("source".into(), b.clone(), "hello".into())
            .unwrap();
        drop(outbox);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        assert!(matches!(
            outbox.state.messages[0].status,
            Status::Queued { .. }
        ));
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        let _accepted_but_not_saved = outbox.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(outbox);
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn full_save_failure_and_corruption_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let mut outbox = Outbox::open(&path, 1, 1, 1).unwrap();
        outbox
            .enqueue("source".into(), b.clone(), "hello".into())
            .unwrap();
        assert!(
            outbox
                .enqueue("source".into(), b.clone(), "other".into())
                .is_err()
        );
        fs::remove_dir_all(dir.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(
            outbox
                .schedule(
                    &HashMap::from([(b.room.clone(), b)]),
                    false,
                    false,
                    &[],
                    fake(&calls, accepted)
                )
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(outbox);
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(&path, b"broken").unwrap();
        assert!(Outbox::open(&path, 1, 1, 1).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken");
    }

    #[test]
    fn pause_fuse_drop_hold_and_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 3, 2, 2).unwrap();
        outbox
            .enqueue("source".into(), b.clone(), "one".into())
            .unwrap();
        outbox
            .schedule(&bindings, true, false, &[], fake(&calls, dropped))
            .unwrap();
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Queued { hold: None }
        );
        outbox
            .schedule(&bindings, false, true, &[], fake(&calls, dropped))
            .unwrap();
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Queued { hold: None }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, dropped))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert!(matches!(
            outbox.state.messages[0].status,
            Status::Queued { hold: Some(_) }
        ));
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, dropped))
            .unwrap();
        assert!(matches!(
            outbox.state.messages[0].status,
            Status::Queued { hold: Some(_) }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let c = binding("b", 1);
        let mut bindings = bindings;
        bindings.insert(c.room.clone(), c.clone());
        outbox
            .enqueue("source".into(), c.clone(), "two".into())
            .unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        bindings.insert(c.room.clone(), binding("b", 2));
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[1].status, Status::Uncertain);
    }

    #[test]
    fn lanes_remain_ordered_and_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let a = binding("a", 1);
        let b = binding("b", 1);
        let bindings = HashMap::from([(a.room.clone(), a.clone()), (b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 2, 2).unwrap();
        assert!(Outbox::open(&path, 4, 2, 2).is_err());
        outbox
            .enqueue("s".into(), a.clone(), "first".into())
            .unwrap();
        outbox.enqueue("s".into(), a, "second".into()).unwrap();
        outbox.enqueue("s".into(), b, "other".into()).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, dropped))
            .unwrap();
        while !outbox.active.is_empty() {
            wait(&mut outbox, &bindings);
        }
        assert!(matches!(
            outbox.state.messages[0].status,
            Status::Queued { hold: Some(_) }
        ));
        assert!(matches!(
            outbox.state.messages[1].status,
            Status::Queued { hold: None }
        ));
        assert!(matches!(
            outbox.state.messages[2].status,
            Status::Queued { hold: Some(_) }
        ));
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(
            outbox.state.messages[1].status,
            Status::Queued { hold: None }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn mail_for_an_earlier_launch_is_held_with_a_reason_and_does_not_block_or_fill_the_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let old = binding("a", 1);
        let new = binding("a", 2);
        let current = HashMap::from([(new.room.clone(), new.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 2, 2).unwrap();
        outbox
            .enqueue("s".into(), old.clone(), "stale one".into())
            .unwrap();
        outbox
            .enqueue("s".into(), old.clone(), "stale two".into())
            .unwrap();
        // The outbox is full of live work until the launch changes.
        assert!(
            outbox
                .enqueue("s".into(), new.clone(), "fresh".into())
                .is_err()
        );

        outbox
            .schedule(&current, false, false, &[], fake(&calls, accepted))
            .unwrap();
        let held = Status::Queued {
            hold: Some(STALE_HOLD.to_owned()),
        };
        assert_eq!(outbox.state.messages[0].status, held);
        assert_eq!(outbox.state.messages[1].status, held);

        // Two stale messages and a capacity of two: fresh mail is still taken.
        let fresh = outbox
            .enqueue("s".into(), new.clone(), "fresh".into())
            .unwrap()
            .id;
        outbox
            .enqueue("s".into(), new.clone(), "fresh two".into())
            .unwrap();
        assert!(
            outbox
                .enqueue("s".into(), new.clone(), "too many".into())
                .is_err()
        );
        outbox
            .schedule(&current, false, false, &[], fake(&calls, accepted))
            .unwrap();
        // The stale mail does not stand in front of it, and is never sent.
        assert_eq!(outbox.state.messages[2].id, fresh);
        assert_eq!(outbox.state.messages[2].status, Status::Sending);
        wait(&mut outbox, &current);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The hold survives a restart and the mail is still not sent.
        drop(outbox);
        let mut outbox = Outbox::open(&path, 4, 2, 2).unwrap();
        assert_eq!(outbox.state.messages[0].status, held);
        assert_eq!(outbox.state.messages[1].status, held);
        outbox
            .schedule(&current, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(outbox.state.messages[0].status, held);
        assert_eq!(outbox.state.messages[1].status, held);
    }

    #[test]
    fn mail_held_for_earlier_launches_has_its_own_bound_and_is_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let old = binding("a", 1);
        let new = binding("a", 2);
        let current = HashMap::from([(new.room.clone(), new.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 1, 2).unwrap();
        outbox.stale_held_limit = 2;
        outbox
            .enqueue("s".into(), old.clone(), "one".into())
            .unwrap();
        outbox
            .enqueue("s".into(), old.clone(), "two".into())
            .unwrap();
        outbox
            .schedule(&current, false, false, &[], fake(&calls, accepted))
            .unwrap();
        let error = outbox.enqueue("s".into(), new, "fresh".into()).unwrap_err();
        assert!(error.to_string().contains("earlier launches"), "{error}");
        // Refusing is the only effect: both held messages are still there.
        assert_eq!(outbox.state.messages.len(), 2);
        assert!(outbox.state.messages.iter().all(is_stale_hold));
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_as_private_as_the_state_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        drop(Outbox::open(&path, 2, 2, 1).unwrap());
        assert_eq!(mode(&path.with_extension("lock")), 0o600);
        // A lock file left by an older build with looser permissions is tightened.
        fs::set_permissions(
            path.with_extension("lock"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        drop(Outbox::open(&path, 2, 2, 1).unwrap());
        assert_eq!(mode(&path.with_extension("lock")), 0o600);
    }

    #[test]
    fn accepted_mail_is_history_so_the_outbox_never_fills_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 3, 1).unwrap();
        for n in 0..10 {
            outbox
                .enqueue("s".into(), b.clone(), format!("m{n}"))
                .unwrap();
            outbox
                .schedule(&bindings, false, false, &[], fake(&calls, accepted))
                .unwrap();
            wait(&mut outbox, &bindings);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 10);
        // Only the newest three finished messages are kept.
        assert_eq!(outbox.state.messages.len(), 3);
        assert_eq!(outbox.state.messages[2].text, "m9");

        // Restarting keeps that: more than the capacity still goes through.
        drop(outbox);
        let mut outbox = Outbox::open(&path, 2, 3, 1).unwrap();
        for n in 10..16 {
            outbox
                .enqueue("s".into(), b.clone(), format!("m{n}"))
                .unwrap();
            outbox
                .schedule(&bindings, false, false, &[], fake(&calls, accepted))
                .unwrap();
            wait(&mut outbox, &bindings);
        }
        assert_eq!(outbox.state.messages.len(), 3);

        // Work that is not finished is never evicted to make room.
        outbox
            .enqueue("s".into(), b.clone(), "live one".into())
            .unwrap();
        outbox
            .enqueue("s".into(), b.clone(), "live two".into())
            .unwrap();
        assert!(
            outbox
                .enqueue("s".into(), b.clone(), "no room".into())
                .is_err()
        );
        assert!(outbox.state.messages.iter().any(|m| m.text == "live one"));
        assert!(outbox.state.messages.iter().any(|m| m.text == "live two"));
    }

    #[test]
    fn accepted_history_is_trimmed_when_a_reopened_outbox_takes_new_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let mut outbox = Outbox::open(&path, 50, 50, 1).unwrap();
            for n in 0..8 {
                outbox
                    .enqueue("s".into(), b.clone(), format!("m{n}"))
                    .unwrap();
                outbox
                    .schedule(&bindings, false, false, &[], fake(&calls, accepted))
                    .unwrap();
                wait(&mut outbox, &bindings);
            }
            assert_eq!(outbox.state.messages.len(), 8);
        }
        // An older run with a larger history left more than this run allows.
        let mut outbox = Outbox::open(&path, 2, 2, 1).unwrap();
        outbox.enqueue("s".into(), b, "new".into()).unwrap();
        assert_eq!(outbox.state.messages.len(), 3);
    }

    #[test]
    fn a_message_whose_save_was_not_confirmed_is_never_sent_and_is_held_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        outbox.sync_parent = |_| Err(io::Error::other("injected directory sync failure"));
        let error = outbox
            .enqueue("s".into(), b.clone(), "maybe saved".into())
            .unwrap_err();
        // The error names the message, so a caller can reconcile instead of retrying.
        let detail = error
            .get_ref()
            .unwrap()
            .downcast_ref::<DurabilityUncertain>()
            .unwrap();
        let id = detail.id.clone();
        let held = Status::Queued {
            hold: Some(UNCONFIRMED_HOLD.to_owned()),
        };
        assert_eq!(outbox.state.messages[0].id, id);
        assert_eq!(outbox.state.messages[0].status, held);

        // Later passes send nothing.
        outbox.sync_parent = sync_parent;
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(outbox.state.messages[0].status, held);

        // What is on disk says the same after a restart.
        drop(outbox);
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        assert_eq!(outbox.state.messages[0].id, id);
        assert_eq!(outbox.state.messages[0].status, held);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_save_that_only_fails_the_last_directory_sync_is_reported_as_success_and_sent_once() {
        use std::sync::atomic::AtomicBool;
        static FAIL_NEXT: AtomicBool = AtomicBool::new(false);
        static SECOND_SAVE: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        FAIL_NEXT.store(true, Ordering::SeqCst);
        SECOND_SAVE.store(0, Ordering::SeqCst);
        // Succeeds on the first save of the enqueue, fails on the release.
        outbox.sync_parent = |_| {
            if FAIL_NEXT.load(Ordering::SeqCst) && SECOND_SAVE.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(io::Error::other("injected"));
            }
            Ok(())
        };
        let id = outbox
            .enqueue("s".into(), b.clone(), "once".into())
            .unwrap()
            .id;
        assert_eq!(outbox.state.messages[0].id, id);
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Queued { hold: None }
        );
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_release_that_fails_before_the_file_is_replaced_leaves_the_message_held_and_reports_it() {
        use std::sync::Mutex as StdMutex;
        static VANISH: StdMutex<Option<PathBuf>> = StdMutex::new(None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        *VANISH.lock().unwrap() = Some(dir.path().join("state"));
        // After the first save the directory disappears, so the release cannot be written.
        outbox.sync_parent = |_| {
            if let Some(gone) = VANISH.lock().unwrap().take() {
                fs::remove_dir_all(gone).unwrap();
            }
            Ok(())
        };
        let error = outbox
            .enqueue("s".into(), b.clone(), "stuck".into())
            .unwrap_err();
        assert!(
            error
                .get_ref()
                .is_none_or(|inner| !inner.is::<DurabilityUncertain>())
        );
        // It is held in memory and never sent.
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Queued {
                hold: Some(UNCONFIRMED_HOLD.to_owned())
            }
        );
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap_or_default();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_a_matching_uncertain_message_can_be_settled_and_settling_frees_its_lane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 2).unwrap();
        let first = outbox
            .enqueue("s".into(), b.clone(), "first".into())
            .unwrap()
            .id;
        outbox
            .enqueue("s".into(), b.clone(), "second".into())
            .unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        let hash = outbox.state.messages[0].hash.clone();

        // The second message is not held behind the uncertain one.
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(outbox.state.messages[1].status, Status::Sending);
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[1].status, Status::Accepted);

        // Wrong evidence settles nothing.
        let accepted_outcome = || Resolution::Accepted(Some("receipt".into()));
        assert!(
            outbox
                .resolve("missing", &b, &hash, accepted_outcome())
                .is_err()
        );
        assert!(
            outbox
                .resolve(&first, &binding("a", 2), &hash, accepted_outcome())
                .is_err()
        );
        assert!(
            outbox
                .resolve(&first, &b, "wrong hash", accepted_outcome())
                .is_err()
        );
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);

        outbox
            .resolve(&first, &b, &hash, accepted_outcome())
            .unwrap();
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        assert_eq!(outbox.state.messages[0].receipt.as_deref(), Some("receipt"));
        // A replay with the same outcome re-saves the row so a partial save can
        // land; a different outcome is still refused.
        outbox
            .resolve(&first, &b, &hash, accepted_outcome())
            .expect("the same settle must replay");
        assert!(
            outbox
                .resolve(
                    &first,
                    &b,
                    &hash,
                    Resolution::Accepted(Some("other".into()))
                )
                .is_err(),
            "a different outcome must not overwrite a settled row"
        );

        // Settling resent nothing: one attempt for the first, one for the second.
        assert_eq!(outbox.state.messages[0].attempts.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A late 'started' receipt settles an Uncertain row Accepted exactly once
    /// with a single adapter call, and the settlement survives a reopen; a
    /// late 'held' receipt settles Failed with the mod's reason.
    #[test]
    fn a_late_receipt_settles_an_uncertain_row_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 2).unwrap();
        let id = outbox
            .enqueue("s".into(), b.clone(), "late".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        let hash = outbox.state.messages[0].hash.clone();

        outbox
            .resolve(
                &id,
                &b,
                &hash,
                Resolution::Accepted(Some("submitted".into())),
            )
            .unwrap();
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        assert_eq!(outbox.state.messages[0].attempts.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(outbox);
        let mut outbox = Outbox::open(&path, 4, 4, 2).unwrap();
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        assert_eq!(outbox.state.messages[0].attempts.len(), 1);

        let held = outbox
            .enqueue("s".into(), b.clone(), "slow hold".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        let held_hash = outbox
            .state
            .messages
            .iter()
            .find(|m| m.id == held)
            .unwrap()
            .hash
            .clone();
        outbox
            .resolve(
                &held,
                &b,
                &held_hash,
                Resolution::Failed("prompt is open".into()),
            )
            .unwrap();
        assert!(matches!(
            outbox
                .state
                .messages
                .iter()
                .find(|m| m.id == held)
                .unwrap()
                .status,
            Status::Failed { .. }
        ));
    }

    /// A resolve whose directory sync fails after the file was replaced leaves
    /// the row Accepted in memory. The same settle replays, re-saves and reports
    /// Ok, so the router drops the evidence only after it durably landed.
    #[test]
    fn resolve_replays_after_a_partial_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let id = outbox
            .enqueue("s".into(), b.clone(), "late".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        let hash = outbox.state.messages[0].hash.clone();
        let resolution = || Resolution::Accepted(Some("submitted".into()));

        outbox.sync_parent = |_| Err(io::Error::other("injected directory sync failure"));
        let error = outbox.resolve(&id, &b, &hash, resolution()).unwrap_err();
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<DurabilityUncertain>()
                .is_some()
        );
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Accepted,
            "memory tracks the renamed file"
        );

        outbox.sync_parent = sync_parent;
        outbox
            .resolve(&id, &b, &hash, resolution())
            .expect("resolve must replay after a partial save");
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        drop(outbox);
        let reopened = Outbox::open(&path, 4, 4, 1).unwrap();
        assert_eq!(reopened.state.messages[0].status, Status::Accepted);
        assert_eq!(
            reopened.state.messages[0].receipt.as_deref(),
            Some("submitted")
        );
        assert_eq!(reopened.state.messages[0].attempts.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "settling never resends");
    }

    /// Mail the mod never collected is held, not uncertain: it never reached
    /// the room. A later schedule offers it once when the room can receive.
    #[test]
    fn an_uncollected_mod_hold_is_retried_once_when_the_room_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let mut message = message_with_hold("a", 1, Some(crate::native_session::UNCOLLECTED_HOLD));
        message.binding = b.clone();
        message.hash = format!("{:x}", Sha256::digest(message.text.as_bytes()));
        let id = message.id.clone();
        let mut state = outbox.state.clone();
        state.messages.push(message);
        outbox.update(state, &id).unwrap();
        drop(outbox);

        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the uncollected mail is offered again once"
        );
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "and never sent twice");
    }

    #[test]
    fn uncertain_mail_of_an_earlier_launch_does_not_hold_this_launchs_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 3, 2, 1).unwrap();
        let old_launch = Binding {
            session: "gone".into(),
            ..binding("a", 1)
        };
        let current = Binding {
            session: "live".into(),
            ..binding("a", 2)
        };
        let bindings = HashMap::from([("a".to_owned(), current.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        // A persisted message of an earlier launch that may already have arrived.
        let old = outbox
            .enqueue("s".into(), old_launch, "old".into())
            .unwrap()
            .id;
        let mut state = outbox.state.clone();
        state.messages[0].status = Status::Uncertain;
        outbox.update(state, &old).unwrap();
        // This launch's startup intro, then F4.
        outbox
            .enqueue("s".into(), current.clone(), "intro".into())
            .unwrap();
        outbox
            .enqueue("s".into(), current.clone(), "f4".into())
            .unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        assert_eq!(outbox.state.messages[1].status, Status::Accepted);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[2].status, Status::Accepted);
        // The old message stays uncertain and is never resent.
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn uncertain_mail_of_this_launch_does_not_hold_newer_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 3, 2, 1).unwrap();
        let current = Binding {
            session: "live".into(),
            ..binding("a", 2)
        };
        let bindings = HashMap::from([("a".to_owned(), current.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .enqueue("s".into(), current.clone(), "uncertain".into())
            .unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        outbox.enqueue("s".into(), current, "later".into()).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        assert_eq!(outbox.state.messages[1].status, Status::Accepted);
        assert_eq!(outbox.state.messages[0].attempts.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Mail an older build held behind an uncertain message was never sent, so
    /// it is released and sent instead of staying held forever.
    #[test]
    fn mail_held_behind_an_uncertain_message_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 3, 2, 1).unwrap();
        let b = binding("a", 1);
        let bindings = HashMap::from([("a".to_owned(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let id = outbox
            .enqueue("s".into(), b.clone(), "held".into())
            .unwrap()
            .id;
        let mut state = outbox.state.clone();
        state.messages[0].status = Status::Queued {
            hold: Some(UNCERTAIN_HOLD.to_owned()),
        };
        outbox.update(state, &id).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(outbox.state.messages[0].status, Status::Sending);
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Accepted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn uncertain_mail_does_not_hold_another_rooms_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 4, 4, 2).unwrap();
        let a = binding("a", 1);
        let b = binding("b", 1);
        let bindings = HashMap::from([("a".to_owned(), a.clone()), ("b".to_owned(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        outbox.enqueue("s".into(), a, "lost".into()).unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || AdapterResult::Uncertain),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        outbox.enqueue("s".into(), b, "other".into()).unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[0].status, Status::Uncertain);
        assert_eq!(outbox.state.messages[1].status, Status::Accepted);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn acknowledged_history_is_bounded_and_receipts_are_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 1, 1, 1).unwrap();
        for text in ["first", "second"] {
            let id = outbox
                .enqueue("s".into(), b.clone(), text.into())
                .unwrap()
                .id;
            outbox
                .schedule(&bindings, false, false, &[], fake(&calls, accepted))
                .unwrap();
            wait(&mut outbox, &bindings);
            let hash = outbox
                .state
                .messages
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .hash
                .clone();
            assert!(outbox.acknowledge(&id, &binding("a", 2), &hash).is_err());
            assert!(outbox.acknowledge(&id, &b, "wrong hash").is_err());
            outbox.acknowledge(&id, &b, &hash).unwrap();
        }
        assert_eq!(outbox.state.messages.len(), 1);
        assert_eq!(outbox.state.messages[0].text, "second");
    }

    #[test]
    fn history_never_evicts_undelivered_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 8, 1, 2).unwrap();
        for (room, text) in [("u", "uncertain"), ("s", "sending"), ("q", "queued")] {
            outbox
                .enqueue("source".into(), binding(room, 1), text.into())
                .unwrap();
        }
        let mut state = outbox.state.clone();
        state.messages[0].status = Status::Uncertain;
        state.messages[1].status = Status::Sending;
        outbox.update(state, "setup").unwrap();
        for text in ["first history", "second history"] {
            let b = binding("history", 1);
            let id = outbox
                .enqueue("source".into(), b.clone(), text.into())
                .unwrap()
                .id;
            let mut state = outbox.state.clone();
            let item = state.messages.iter_mut().find(|m| m.id == id).unwrap();
            item.status = Status::Accepted;
            let hash = item.hash.clone();
            outbox.update(state, &id).unwrap();
            outbox.acknowledge(&id, &b, &hash).unwrap();
        }
        for (text, status) in [
            ("uncertain", Status::Uncertain),
            ("sending", Status::Sending),
            ("queued", Status::Queued { hold: None }),
        ] {
            assert!(
                outbox
                    .state
                    .messages
                    .iter()
                    .any(|m| m.text == text && m.status == status)
            );
        }
    }

    #[test]
    fn binding_panic_and_uncertain_lane_guards() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = Outbox::open(&dir.path().join("outbox.json"), 8, 1, 2).unwrap();
        let stale = binding("a", 1);
        let current = binding("a", 2);
        let bindings = HashMap::from([(current.room.clone(), current.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        outbox
            .enqueue("s".into(), stale.clone(), "first".into())
            .unwrap();
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        // A message for another launch is held with a reason, not sent.
        assert_eq!(
            outbox.state.messages[0].status,
            Status::Queued {
                hold: Some(STALE_HOLD.to_owned())
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // An adapter that panics leaves the message uncertain, and the mail
        // behind it still goes out.
        outbox
            .enqueue("s".into(), current.clone(), "second".into())
            .unwrap();
        outbox.enqueue("s".into(), current, "third".into()).unwrap();
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &[],
                fake(&calls, || panic!("adapter")),
            )
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[1].status, Status::Uncertain);
        outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap();
        assert_eq!(outbox.state.messages[2].status, Status::Sending);
        wait(&mut outbox, &bindings);
        assert_eq!(outbox.state.messages[2].status, Status::Accepted);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn directory_sync_failure_keeps_persisted_state_and_prevents_send() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let b = binding("a", 1);
        let bindings = HashMap::from([(b.room.clone(), b.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 2, 1, 1).unwrap();
        let id = outbox.enqueue("s".into(), b, "keep me".into()).unwrap().id;
        outbox.sync_parent = |_| Err(io::Error::other("injected directory sync failure"));
        let error = outbox
            .schedule(&bindings, false, false, &[], fake(&calls, accepted))
            .unwrap_err();
        let detail = error
            .get_ref()
            .unwrap()
            .downcast_ref::<DurabilityUncertain>()
            .unwrap();
        assert_eq!(detail.id, id);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(outbox.state.messages[0].status, Status::Sending);
        let disk: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&outbox.state).unwrap(), disk);
        drop(outbox);
        let reopened = Outbox::open(&path, 2, 1, 1).unwrap();
        assert_eq!(reopened.state.messages[0].id, id);
        assert_eq!(reopened.state.messages[0].text, "keep me");
        assert_eq!(reopened.state.messages[0].status, Status::Uncertain);
    }

    /// Decision table for the native session and hold rules: one row per hold
    /// reason and one row per 0.46.x bug, over the current predicates. Step C
    /// moves these predicates verbatim into native_session.rs; this table
    /// stays green through every move.
    fn message_with_hold(room: &str, generation: u64, hold: Option<&str>) -> Message {
        Message {
            id: format!("row-{room}-{generation}"),
            source: "table".into(),
            destination: room.into(),
            binding: Binding {
                room: room.into(),
                session: "session".into(),
                endpoint: "endpoint".into(),
                generation,
            },
            text: "row".into(),
            hash: "row".into(),
            created_ms: 0,
            sequence: 1,
            attempts: Vec::new(),
            receipt: None,
            status: Status::Queued {
                hold: hold.map(str::to_owned),
            },
        }
    }

    #[test]
    fn hold_reasons_classify_stale_and_retryable() {
        struct Row {
            hold: Option<&'static str>,
            retryable: bool,
            stale: bool,
        }
        let rows = [
            Row {
                hold: None,
                retryable: false,
                stale: false,
            },
            Row {
                hold: Some(STALE_HOLD),
                retryable: false,
                stale: true,
            },
            Row {
                hold: Some(PENDING_HOLD),
                retryable: false,
                stale: false,
            },
            Row {
                hold: Some(CLAUDE_HOLD),
                retryable: true,
                stale: false,
            },
            Row {
                hold: Some(UNCERTAIN_HOLD),
                retryable: true,
                stale: false,
            },
            Row {
                hold: Some(UNCONFIRMED_HOLD),
                retryable: false,
                stale: false,
            },
            Row {
                hold: Some(crate::native_session::UNCOLLECTED_HOLD),
                retryable: true,
                stale: false,
            },
        ];
        for row in rows {
            let message = message_with_hold("a", 1, row.hold);
            assert_eq!(
                is_stale_hold(&message),
                row.stale,
                "stale for {:?}",
                row.hold
            );
            assert_eq!(
                is_retryable_hold(row.hold.unwrap_or("")),
                row.retryable,
                "retryable for {:?}",
                row.hold
            );
        }
    }

    #[test]
    fn ab39d84_first_report_keeps_its_launch_generation() {
        // The launch placeholder names one launch, never a session; the real
        // session that replaces it on the first report keeps the launch
        // generation, so mail saved against the placeholder still belongs to
        // this launch and a later session switch takes a new generation.
        assert_eq!(unreported_session("1", 5), "unreported-1-5");
        let placeholder = Binding {
            room: "1".into(),
            session: unreported_session("1", 5),
            endpoint: "claude".into(),
            generation: 5,
        };
        assert!(is_unreported_placeholder(&placeholder));
        let reported = Binding {
            room: "1".into(),
            session: "8c3f4b96-3b74-4021-9f83-2c3d4e5f6071".into(),
            endpoint: "claude".into(),
            generation: 5,
        };
        assert!(!is_unreported_placeholder(&reported));
        struct Row {
            held: Binding,
            current: Binding,
            same_launch: bool,
        }
        let rows = [
            // First report keeps the generation: the placeholder's mail moves.
            Row {
                held: placeholder.clone(),
                current: reported.clone(),
                same_launch: true,
            },
            // Older generation: mail belongs to a launch that is gone.
            Row {
                held: placeholder.clone(),
                current: Binding {
                    generation: 6,
                    ..reported.clone()
                },
                same_launch: false,
            },
            // A real session is never a launch placeholder.
            Row {
                held: reported.clone(),
                current: reported.clone(),
                same_launch: false,
            },
            // Another room is another launch even with the same generation.
            Row {
                held: placeholder.clone(),
                current: Binding {
                    room: "2".into(),
                    ..reported.clone()
                },
                same_launch: false,
            },
        ];
        for row in rows {
            assert_eq!(
                same_launch_placeholder(&row.held, &row.current),
                row.same_launch,
                "held {:?} current {:?}",
                row.held,
                row.current
            );
        }
    }

    #[test]
    fn retarget_never_moves_mail_for_another_launch_83c281b() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let current = binding("a", 1);
        let bindings = HashMap::from([("a".to_owned(), current.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        // Mail saved before the pending mark: the mark holds it as stale.
        let old = outbox
            .enqueue("source".into(), current.clone(), "old".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        assert_eq!(hold_of(&mut outbox, &old), Some(STALE_HOLD.to_owned()));
        // Mail saved while the mark is set: held for the next session.
        let new = outbox
            .enqueue("source".into(), current.clone(), "new".into())
            .unwrap()
            .id;
        outbox
            .schedule(
                &bindings,
                false,
                false,
                &["a".to_owned()],
                fake(&calls, accepted),
            )
            .unwrap();
        assert_eq!(hold_of(&mut outbox, &new), Some(PENDING_HOLD.to_owned()));
        // A new generation adopts only the pending-held mail.
        let next = pending_binding("a", 2);
        outbox.retarget("a", &next).unwrap();
        for (id, hold, want) in [(&old, Some(STALE_HOLD), &current), (&new, None, &next)] {
            let message = outbox
                .state
                .messages
                .iter()
                .find(|message| &message.id == id)
                .unwrap();
            assert_eq!(
                message.status,
                Status::Queued {
                    hold: hold.map(str::to_owned)
                },
                "hold for {id}"
            );
            assert_eq!(&message.binding, want, "binding for {id}");
        }
        // ...and the stale mail is never sent: only the adopted mail goes out.
        let live = HashMap::from([("a".to_owned(), next)]);
        outbox
            .schedule(&live, false, false, &[], fake(&calls, accepted))
            .unwrap();
        wait(&mut outbox, &live);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A gate adapter: not ready until its mod registers, then accepts.
    struct Gate {
        ready: Arc<std::sync::atomic::AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    impl Adapter for Gate {
        fn send(&self, _: Message) -> AdapterResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.ready.load(Ordering::SeqCst) {
                return accepted();
            }
            AdapterResult::Dropped(CLAUDE_HOLD.to_owned())
        }

        fn ready(&self, _: &str) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn retryable_hold_reoffered_once_mod_ready_e069509() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.json");
        let startup = Binding {
            room: "a".to_owned(),
            session: unreported_session("a", 1),
            endpoint: "claude".to_owned(),
            generation: 1,
        };
        let bindings = HashMap::from([("a".to_owned(), startup.clone())]);
        let calls = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let adapter: Arc<dyn Adapter> = Arc::new(Gate {
            ready: Arc::clone(&ready),
            calls: Arc::clone(&calls),
        });
        let mut outbox = Outbox::open(&path, 4, 4, 1).unwrap();
        let id = outbox
            .enqueue("source".into(), startup, "house rules".into())
            .unwrap()
            .id;
        // No mod: one attempt, then held, never retried while missing.
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(hold_of(&mut outbox, &id), Some(CLAUDE_HOLD.to_owned()));
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The mod registers: offered exactly once more, then accepted.
        ready.store(true, Ordering::SeqCst);
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        wait(&mut outbox, &bindings);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // ...and never re-sent afterwards.
        outbox
            .schedule(&bindings, false, false, &[], Arc::clone(&adapter))
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn room_number_reaches_mod_through_files_7245fc1() {
        // The Claude mod sandbox has no process global, so the mod resolves
        // its room number from its per-room connection file, which carries the
        // numeric room. (The mod side is pinned by the node deliver tests;
        // this row pins the Rust side of the contract.)
        let cwd = tempfile::tempdir().unwrap();
        // Bound past the reads: dropping an inbox removes its files.
        let one = crate::claude_inbox::ClaudeInbox::start().unwrap();
        let two = crate::claude_inbox::ClaudeInbox::start().unwrap();
        one.prepare(cwd.path(), 1, 7).unwrap();
        two.prepare(cwd.path(), 2, 7).unwrap();
        for room in [1_usize, 2] {
            let raw = std::fs::read(crate::claude_inbox::bootstrap_file(cwd.path(), room).unwrap())
                .unwrap();
            let connection: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(connection["room"], room);
            assert_ne!(connection["url"].as_str().unwrap_or(""), "");
            assert_ne!(connection["token"].as_str().unwrap_or(""), "");
        }
        let first =
            std::fs::read(crate::claude_inbox::bootstrap_file(cwd.path(), 1).unwrap()).unwrap();
        let second =
            std::fs::read(crate::claude_inbox::bootstrap_file(cwd.path(), 2).unwrap()).unwrap();
        assert_ne!(first, second, "each room gets its own connection");
    }
}

/// Mail held for an earlier launch never counts toward the live capacity, so
/// this separate bound keeps it from growing without limit. Nothing is evicted;
/// reaching it refuses new mail with a clear error.
const STALE_HELD_LIMIT: usize = 1000;

/// Finished work kept only as history: it never counts as live work.
fn is_history(status: &Status) -> bool {
    matches!(
        status,
        Status::Accepted | Status::Acknowledged | Status::Failed { .. }
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Status {
    Queued { hold: Option<String> },
    Sending,
    Accepted,
    Acknowledged,
    Uncertain,
    Failed { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Attempt {
    pub number: u64,
    pub started_ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Message {
    pub id: String,
    pub source: String,
    pub destination: String,
    pub binding: Binding,
    pub text: String,
    pub hash: String,
    pub created_ms: u128,
    pub sequence: u64,
    pub attempts: Vec<Attempt>,
    pub receipt: Option<String>,
    pub status: Status,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    messages: Vec<Message>,
    sequences: HashMap<String, u64>,
}

fn now() -> io::Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis())
}

pub(crate) enum AdapterResult {
    Accepted(Option<String>),
    Dropped(String),
    Uncertain,
    Failed(String),
}

pub(crate) trait Adapter: Send + Sync + 'static {
    fn send(&self, message: Message) -> AdapterResult;

    /// Whether the room can take mail now. A room that cannot keeps its mail
    /// held instead of being retried on every schedule.
    fn ready(&self, _room: &str) -> bool {
        true
    }
}

pub(crate) enum Command {
    Enqueue {
        source: String,
        binding: Binding,
        text: String,
    },
    Schedule {
        bindings: HashMap<String, Binding>,
        paused: bool,
        fused: bool,
        /// Rooms that reported themselves between sessions. Their new mail is
        /// held instead of sent; see [`PENDING_HOLD`].
        pending: Vec<String>,
    },
    /// Replaces the rooms considered between-sessions. Processed in command
    /// order, so a report's mark always precedes mail the router saves after
    /// it; periodic schedules repeat the same set.
    SetPending { rooms: Vec<String> },
    /// Adopts a pending room's held mail onto a binding. Only messages held
    /// with [`PENDING_HOLD`] move; mail held for any other reason stays put.
    Retarget { room: String, binding: Binding },
    Acknowledge {
        id: String,
        binding: Binding,
        hash: String,
    },
    /// Settles an uncertain message from evidence gathered outside the outbox.
    Resolve {
        id: String,
        binding: Binding,
        hash: String,
        outcome: Resolution,
    },
}

/// How an uncertain message was settled by outside evidence.
pub(crate) enum Resolution {
    /// A read-only check of the native inbox found the exact message.
    Accepted(Option<String>),
    Failed(String),
}

/// What the outbox reports for a message it just saved: the id it was saved
/// under and the status it was saved with. The status travels with the id so
/// a hold assigned at enqueue shows immediately instead of at the next
/// scheduling snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedMail {
    pub(crate) id: String,
    pub(crate) status: Status,
}

pub(crate) enum Event {
    Queued(QueuedMail),
    Updated(Vec<Message>),
    Error(String),
    /// An error from a hold command, carrying the rooms it was about. The
    /// pending and retarget commands have no entry in the router's waiting
    /// queue, so they answer for themselves instead of taking the reply of
    /// whichever command happens to be waiting next.
    HoldFailed {
        rooms: Vec<String>,
        error: String,
    },
}

/// One command, with the id the router gave it. The worker answers under
/// that id, so a response reaches the command that asked for it instead of
/// whichever command happens to be waiting.
pub(crate) struct TaggedCommand {
    pub(crate) id: u64,
    pub(crate) command: Command,
}

/// One event, carrying the id of the command that produced it. Id 0 is
/// reserved for a failure that no command is behind, such as a state file
/// that could not be opened.
pub(crate) struct TaggedEvent {
    pub(crate) id: u64,
    pub(crate) event: Event,
}

/// Test-only: the one outbox path whose next pending-hold write must fail, so
/// a transient persistence failure can be driven without breaking the disk.
/// Keyed by path and armed for a single write, so only the test that asked
/// for it is affected and every other outbox keeps writing normally.
#[cfg(test)]
static PENDING_HOLD_FAILURE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Test-only: reports and disarms a pending-hold failure armed for `path`.
#[cfg(test)]
fn take_pending_hold_failure(path: &Path) -> bool {
    let mut guard = PENDING_HOLD_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.as_deref() == Some(path) {
        *guard = None;
        return true;
    }
    false
}

/// Test-only: makes the next pending-hold write for this outbox fail.
#[cfg(test)]
pub(crate) fn fail_pending_hold_at(path: &Path) {
    let mut guard = PENDING_HOLD_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(path.to_path_buf());
}

/// Test-only: the one outbox path whose next schedule must fail, so a
/// schedule error can be driven while every other command still succeeds.
/// Keyed by path and armed for a single command, like the hold failure.
#[cfg(test)]
static SCHEDULE_FAILURE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Test-only: reports and disarms a schedule failure armed for `path`.
#[cfg(test)]
fn take_schedule_failure(path: &Path) -> bool {
    let mut guard = SCHEDULE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.as_deref() == Some(path) {
        *guard = None;
        return true;
    }
    false
}

/// Test-only: makes the next schedule for this outbox fail.
#[cfg(test)]
pub(crate) fn fail_next_schedule_at(path: &Path) {
    let mut guard = SCHEDULE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(path.to_path_buf());
}

/// Disk and adapter work run outside the terminal thread. A full command
/// channel rejects intake instead of blocking the caller.
pub(crate) struct BackgroundOutbox {
    commands: mpsc::SyncSender<TaggedCommand>,
    pub events: mpsc::Receiver<TaggedEvent>,
}

impl BackgroundOutbox {
    pub fn try_send(
        &self,
        id: u64,
        command: Command,
    ) -> Result<(), mpsc::TrySendError<TaggedCommand>> {
        self.commands.try_send(TaggedCommand { id, command })
    }

    pub fn start(
        path: PathBuf,
        capacity: usize,
        history: usize,
        max_in_flight: usize,
        adapter: Arc<dyn Adapter>,
    ) -> Self {
        let (commands, input) = mpsc::sync_channel(64);
        let (output, events) = mpsc::sync_channel(64);
        thread::spawn(move || {
            let mut outbox = match Outbox::open(&path, capacity, history, max_in_flight) {
                Ok(outbox) => outbox,
                Err(error) => {
                    let _ = output.send(TaggedEvent {
                        id: 0,
                        event: Event::Error(error.to_string()),
                    });
                    return;
                }
            };
            while let Ok(TaggedCommand { id, command }) = input.recv() {
                // The hold commands answer for themselves: their failure
                // carries the rooms it was about instead of taking the reply
                // of whichever command is waiting next.
                let hold_rooms = match &command {
                    Command::SetPending { rooms } => Some(rooms.clone()),
                    Command::Retarget { room, .. } => Some(vec![room.clone()]),
                    _ => None,
                };
                let result = match command {
                    Command::Enqueue {
                        source,
                        binding,
                        text,
                    } => outbox.enqueue(source, binding, text).map(Event::Queued),
                    Command::Schedule {
                        bindings,
                        paused,
                        fused,
                        pending,
                    } => outbox
                        .schedule(&bindings, paused, fused, &pending, adapter.clone())
                        .map(|()| Event::Updated(outbox.state.messages.clone())),
                    Command::SetPending { rooms } => outbox
                        .set_pending(&rooms)
                        .map(|()| Event::Updated(outbox.state.messages.clone())),
                    Command::Retarget { room, binding } => outbox
                        .retarget(&room, &binding)
                        .map(|()| Event::Updated(outbox.state.messages.clone())),
                    Command::Acknowledge { id, binding, hash } => outbox
                        .acknowledge(&id, &binding, &hash)
                        .map(|()| Event::Updated(outbox.state.messages.clone())),
                    Command::Resolve {
                        id,
                        binding,
                        hash,
                        outcome,
                    } => outbox
                        .resolve(&id, &binding, &hash, outcome)
                        .map(|()| Event::Updated(outbox.state.messages.clone())),
                };
                let event = match result {
                    Ok(event) => event,
                    Err(error) => match hold_rooms {
                        Some(rooms) => Event::HoldFailed {
                            rooms,
                            error: error.to_string(),
                        },
                        None => Event::Error(error.to_string()),
                    },
                };
                if output.send(TaggedEvent { id, event }).is_err() {
                    break;
                }
            }
        });
        Self { commands, events }
    }
}

struct Completion {
    id: String,
    binding: Binding,
    result: AdapterResult,
}

/// Own this object on a background thread; persistence includes fsync.
pub(crate) struct Outbox {
    path: PathBuf,
    _lock: File,
    state: State,
    capacity: usize,
    history: usize,
    max_in_flight: usize,
    stale_held_limit: usize,
    active: HashSet<String>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
    /// Rooms currently reported between sessions. Commands run in order on
    /// one thread, so mail enqueued while a room is listed here is held for
    /// the next session, while mail saved before the mark is stale.
    pending: HashSet<String>,
    /// Id of a state write that failed. The state it failed to write is
    /// already held in memory, so every later pass retries the write until
    /// it lands; the failure itself is reported once, when it happens.
    save_failed: Option<String>,
    #[cfg(test)]
    sync_parent: fn(&Path) -> io::Result<()>,
}

#[derive(Debug)]
struct DurabilityUncertain {
    id: String,
    source: io::Error,
}

impl std::fmt::Display for DurabilityUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "message {} was persisted but durability is uncertain: {}",
            self.id, self.source
        )
    }
}

impl std::error::Error for DurabilityUncertain {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn sync_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

impl Drop for Outbox {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}

impl Outbox {
    pub fn open(
        path: &Path,
        capacity: usize,
        history: usize,
        max_in_flight: usize,
    ) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("outbox has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut lock_options = OpenOptions::new();
        lock_options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let lock = lock_options.open(path.with_extension("lock"))?;
        // A lock file made by an older build may be more open than the state.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        lock.try_lock().map_err(io::Error::other)?;
        let mut state: State = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        let mut ids = HashSet::new();
        if state.messages.iter().any(|m| {
            !ids.insert(&m.id)
                || m.hash != format!("{:x}", Sha256::digest(m.text.as_bytes()))
                || m.destination != m.binding.room
        }) {
            return Err(io::Error::other("invalid outbox state"));
        }
        let recovered = state.messages.iter_mut().fold(false, |changed, m| {
            if m.status == Status::Sending {
                m.status = Status::Uncertain;
                true
            } else {
                changed
            }
        });
        let (tx, rx) = mpsc::channel();
        let outbox = Self {
            path: path.into(),
            _lock: lock,
            state,
            capacity,
            history,
            max_in_flight,
            stale_held_limit: STALE_HELD_LIMIT,
            active: HashSet::new(),
            pending: HashSet::new(),
            save_failed: None,
            tx,
            rx,
            #[cfg(test)]
            sync_parent,
        };
        if recovered {
            if let Some(source) = outbox.save(&outbox.state)? {
                let ids = outbox
                    .state
                    .messages
                    .iter()
                    .filter(|m| m.status == Status::Uncertain)
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                return Err(io::Error::other(DurabilityUncertain { id: ids, source }));
            }
        }
        Ok(outbox)
    }

    fn save(&self, state: &State) -> io::Result<Option<io::Error>> {
        let parent = self.path.parent().unwrap();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut temp, state).map_err(io::Error::other)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        #[cfg(test)]
        let result = (self.sync_parent)(parent);
        #[cfg(not(test))]
        let result = sync_parent(parent);
        Ok(result.err())
    }

    fn update(&mut self, state: State, id: &str) -> io::Result<()> {
        let uncertain = self.save(&state)?;
        self.state = state;
        if let Some(source) = uncertain {
            return Err(io::Error::other(DurabilityUncertain {
                id: id.into(),
                source,
            }));
        }
        Ok(())
    }

    /// Remembers which rooms are between sessions. The set is replaced
    /// wholesale: reports set it eagerly in command order, and every schedule
    /// repeats the router's set, so a restart relearns it on the next tick.
    /// Mail still queued unheld for a room that just reported itself belongs
    /// to the session that ended, so it is classified as stale right here,
    /// where the pending command lands; a later report for the same session
    /// then has nothing to release. Mail saved after the mark carries
    /// [`PENDING_HOLD`] and is left alone.
    ///
    /// The classification reaches memory before the disk write, so a save
    /// that fails still leaves this mail held: a later same-session clear
    /// must never be able to release it to the adapter. The failure is
    /// reported once and the write is retried on every later pass.
    pub fn set_pending(&mut self, pending: &[String]) -> io::Result<()> {
        let rooms: HashSet<String> = pending.iter().cloned().collect();
        let mut next = self.state.clone();
        let mut classified = None;
        for message in next.messages.iter_mut() {
            if rooms.contains(&message.destination)
                && matches!(&message.status, Status::Queued { hold: None })
            {
                message.status = Status::Queued {
                    hold: Some(STALE_HOLD.to_owned()),
                };
                if classified.is_none() {
                    classified = Some(message.id.clone());
                }
            }
        }
        self.pending = rooms;
        if let Some(id) = classified {
            self.state = next;
            return self.persist(&id);
        }
        // A hold that never reached disk keeps being retried here. The
        // failure was already reported when it first happened, and the
        // in-memory hold blocks the mail either way.
        if let Some(id) = self.save_failed.clone() {
            let _ = self.persist(&id);
        }
        Ok(())
    }

    /// Writes the state as it stands in memory, remembering a failure so the
    /// next pass retries it. The caller has already placed in memory the
    /// state it needs held.
    fn persist(&mut self, id: &str) -> io::Result<()> {
        #[cfg(test)]
        if take_pending_hold_failure(&self.path) {
            self.save_failed = Some(id.to_owned());
            return Err(io::Error::other("the pending hold could not be written"));
        }
        let outcome = self.save(&self.state);
        match outcome {
            Ok(None) => {
                self.save_failed = None;
                Ok(())
            }
            Ok(Some(source)) => {
                self.save_failed = Some(id.to_owned());
                Err(io::Error::other(DurabilityUncertain {
                    id: id.to_owned(),
                    source,
                }))
            }
            Err(error) => {
                self.save_failed = Some(id.to_owned());
                Err(error)
            }
        }
    }

    /// Adopts a room's pending-held mail onto a binding: only messages held
    /// with [`PENDING_HOLD`] move, and only onto a binding for the same room,
    /// so the open-time destination invariant always holds. Mail held for any
    /// other reason stays put and is never retargeted.
    pub fn retarget(&mut self, room: &str, binding: &Binding) -> io::Result<()> {
        if binding.room != room {
            return Err(io::Error::other(
                "a pending room's mail is only ever adopted onto its own room",
            ));
        }
        let mut next = self.state.clone();
        let mut moved = false;
        for message in next
            .messages
            .iter_mut()
            .filter(|message| message.destination == room)
        {
            if matches!(&message.status, Status::Queued { hold: Some(reason) } if reason == PENDING_HOLD)
            {
                message.binding = binding.clone();
                message.status = Status::Queued { hold: None };
                moved = true;
            }
        }
        if moved {
            self.update(next, room)?;
        }
        Ok(())
    }

    /// Moves mail held for a room whose mod was missing onto the room's
    /// current binding, so the next schedule can offer it.
    fn adopt_missing_mod(&mut self, id: &str, binding: &Binding) -> io::Result<()> {
        let mut next = self.state.clone();
        let Some(item) = next.messages.iter_mut().find(|message| message.id == id) else {
            return Ok(());
        };
        item.binding = binding.clone();
        item.status = Status::Queued { hold: None };
        self.update(next, id)
    }

    /// Holds a message under `reason` without sending it.
    fn set_hold(&mut self, id: &str, reason: &str) -> io::Result<()> {
        let mut next = self.state.clone();
        let Some(item) = next.messages.iter_mut().find(|message| message.id == id) else {
            return Ok(());
        };
        item.status = Status::Queued {
            hold: Some(reason.to_owned()),
        };
        self.update(next, id)
    }

    /// Trims terminal rows only, never a live, queued or Uncertain one, so such
    /// a row is never absent from a snapshot. The router's late-receipt
    /// reconciliation relies on that: a row missing from a snapshot received
    /// after the evidence was first peeked was trimmed, and its evidence can
    /// be acked.
    fn trim_history(&self, state: &mut State) {
        let mut terminal = state
            .messages
            .iter()
            .filter(|m| is_history(&m.status))
            .count();
        state.messages.retain(|m| {
            if terminal > self.history && is_history(&m.status) {
                terminal -= 1;
                false
            } else {
                true
            }
        });
    }

    pub fn enqueue(
        &mut self,
        source: String,
        binding: Binding,
        text: String,
    ) -> io::Result<QueuedMail> {
        let mut next = self.state.clone();
        // History is bounded on its own, so it can never fill the outbox.
        self.trim_history(&mut next);
        // Mail held for an earlier launch is kept but is not live work.
        let pending = next
            .messages
            .iter()
            .filter(|m| !is_history(&m.status) && !is_stale_hold(m))
            .count();
        if pending >= self.capacity {
            return Err(io::Error::other("outbox full"));
        }
        if next.messages.iter().filter(|m| is_stale_hold(m)).count() >= self.stale_held_limit {
            return Err(io::Error::other(
                "outbox holds too much mail from earlier launches; nothing was dropped",
            ));
        }
        let parent = self.path.parent().unwrap();
        let mut entropy = String::new();
        for _ in 0..4 {
            let file = tempfile::Builder::new()
                .prefix("message-")
                .tempfile_in(parent)?;
            entropy.push_str(&file.path().file_name().unwrap().to_string_lossy());
        }
        let id = format!("{:x}", Sha256::digest(entropy.as_bytes()));
        if next.messages.iter().any(|m| m.id == id) {
            return Err(io::Error::other("message ID collision"));
        }
        let sequence = next.sequences.entry(binding.room.clone()).or_insert(0);
        *sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("sequence exhausted"))?;
        // The hold is decided here, where the message is saved, and travels
        // with the id: the router shows it from the moment it exists instead
        // of waiting for the next scheduling snapshot to carry it.
        let status = Status::Queued {
            hold: self
                .pending
                .contains(&binding.room)
                .then(|| PENDING_HOLD.to_owned()),
        };
        next.messages.push(Message {
            id: id.clone(),
            source,
            destination: binding.room.clone(),
            binding,
            hash: format!("{:x}", Sha256::digest(text.as_bytes())),
            text,
            created_ms: now()?,
            sequence: *sequence,
            attempts: Vec::new(),
            receipt: None,
            // Saved held first: if confirming the save fails, the message can
            // never be sent, in this run or after a restart.
            status: Status::Queued {
                hold: Some(UNCONFIRMED_HOLD.to_owned()),
            },
        });
        self.update(next, &id)?;
        // Release it. The room may already have reported itself between
        // sessions by the time this runs: then the mail is held for the next
        // session instead. Commands run in order on one thread, so this
        // classification can never mistake older mail for newer.
        // A failure before the new file is in place leaves it held with its
        // ID. A failure only of the directory sync after the rename leaves
        // the released file on disk and is not reported: the caller must not
        // retry what will be sent.
        let mut released = self.state.clone();
        if let Some(item) = released.messages.iter_mut().find(|m| m.id == id) {
            item.status = status.clone();
        }
        match self.update(released, &id) {
            Ok(()) => Ok(QueuedMail { id, status }),
            Err(error)
                if error
                    .get_ref()
                    .is_some_and(|inner| inner.is::<DurabilityUncertain>()) =>
            {
                Ok(QueuedMail { id, status })
            }
            Err(error) => Err(error),
        }
    }

    pub fn schedule(
        &mut self,
        bindings: &HashMap<String, Binding>,
        paused: bool,
        fused: bool,
        pending: &[String],
        adapter: Arc<dyn Adapter>,
    ) -> io::Result<()> {
        #[cfg(test)]
        if take_schedule_failure(&self.path) {
            return Err(io::Error::other("injected schedule failure"));
        }
        self.collect(bindings)?;
        self.set_pending(pending)?;
        if paused || fused {
            return Ok(());
        }
        let mut blocked = HashSet::new();
        // Rooms the adapter reported unable to receive, decided once per pass.
        // Their later mail is held, never sent ahead of an older hold, and
        // still gets an answer instead of waiting without one.
        let mut unready = HashSet::new();
        for mut message in self.state.messages.clone() {
            let room = &message.destination;
            if unready.contains(room) {
                if matches!(&message.status, Status::Queued { hold: None })
                    && bindings.get(room) == Some(&message.binding)
                {
                    self.set_hold(&message.id, CLAUDE_HOLD)?;
                }
                continue;
            }
            if blocked.contains(room) {
                continue;
            }
            if matches!(&message.status, Status::Queued { hold: Some(reason) } if is_retryable_hold(reason))
            {
                // The room's mod was not connected when this mail was saved.
                // It may be offered again only to the launch it belongs to:
                // the room's current binding, or the unreported placeholder of
                // this same generation that its real session replaces. Mail
                // bound to an older session or generation is never retargeted;
                // it belongs to a launch that is gone.
                let Some(current) = bindings.get(room) else {
                    continue;
                };
                if &message.binding != current {
                    if !same_launch_placeholder(&message.binding, current) {
                        self.set_hold(&message.id, STALE_HOLD)?;
                        continue;
                    }
                    self.adopt_missing_mod(&message.id, current)?;
                }
                // While the mod cannot receive, the room stays blocked for the
                // rest of this pass: a newer message must never overtake this
                // older held one.
                if !adapter.ready(room) {
                    blocked.insert(room.clone());
                    unready.insert(room.clone());
                    continue;
                }
                message.binding = current.clone();
                message.status = Status::Queued { hold: None };
            }
            match &message.status {
                Status::Queued { hold: None } => {
                    if bindings.get(room) != Some(&message.binding) {
                        // Mail from this launch's own placeholder may move to
                        // the session the placeholder became. Mail for another
                        // launch is held with a visible reason and never sent;
                        // it neither counts as live work nor stands in front of
                        // newer mail.
                        let Some(current) = bindings.get(room) else {
                            self.set_hold(&message.id, STALE_HOLD)?;
                            continue;
                        };
                        if !same_launch_placeholder(&message.binding, current) {
                            self.set_hold(&message.id, STALE_HOLD)?;
                            continue;
                        }
                        self.adopt_missing_mod(&message.id, current)?;
                        message.binding = current.clone();
                    }
                    if self.pending.contains(room) {
                        // The room reported itself between sessions after this
                        // mail was already saved, so the mail belongs to the
                        // ended session: hold it as stale like any other old
                        // mail. Only mail saved while the mark is set carries
                        // the pending hold, so a later report never moves
                        // anything saved before it.
                        let mut next = self.state.clone();
                        if let Some(item) = next.messages.iter_mut().find(|m| m.id == message.id) {
                            item.status = Status::Queued {
                                hold: Some(STALE_HOLD.to_owned()),
                            };
                        }
                        self.update(next, &message.id)?;
                        continue;
                    }
                    blocked.insert(room.clone());
                    if self.active.len() >= self.max_in_flight || self.active.contains(room) {
                        continue;
                    }
                    let mut next = self.state.clone();
                    let item = next
                        .messages
                        .iter_mut()
                        .find(|m| m.id == message.id)
                        .unwrap();
                    item.attempts.push(Attempt {
                        number: item.attempts.len() as u64 + 1,
                        started_ms: now()?,
                    });
                    item.status = Status::Sending;
                    let attempt = item.clone();
                    self.update(next, &message.id)?;
                    self.active.insert(room.clone());
                    let tx = self.tx.clone();
                    let adapter = adapter.clone();
                    thread::spawn(move || {
                        let binding = attempt.binding.clone();
                        let id = attempt.id.clone();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            adapter.send(attempt)
                        }))
                        .unwrap_or(AdapterResult::Uncertain);
                        let _ = tx.send(Completion {
                            id,
                            binding,
                            result,
                        });
                    });
                }
                // Held mail of an earlier launch does not stand in front of newer mail.
                Status::Queued { hold: Some(reason) } if reason == STALE_HOLD => {}
                // A message that may already have arrived is never resent, and
                // it does not hold the room's later mail: nothing can settle it
                // within a launch, so blocking would strand the room until a
                // relaunch. A later message may arrive without it; the
                // uncertain row stays visible.
                Status::Uncertain => {}
                Status::Queued { .. } | Status::Sending => {
                    blocked.insert(room.clone());
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn collect(&mut self, bindings: &HashMap<String, Binding>) -> io::Result<()> {
        while let Ok(completion) = self.rx.try_recv() {
            self.active.remove(&completion.binding.room);
            let mut next = self.state.clone();
            let Some(item) = next.messages.iter_mut().find(|m| m.id == completion.id) else {
                continue;
            };
            if item.status != Status::Sending || item.binding != completion.binding {
                continue;
            }
            let current = bindings.get(&item.destination);
            let same_launch = current.is_some_and(|current| {
                current == &completion.binding
                    || same_launch_placeholder(&completion.binding, current)
            });
            if !same_launch {
                item.status = Status::Uncertain;
            } else {
                // A completion from this launch's placeholder still describes
                // this launch: its first report only renamed the session.
                if let Some(current) = current {
                    item.binding = current.clone();
                }
                item.status = match completion.result {
                    AdapterResult::Accepted(receipt) => {
                        item.receipt = receipt;
                        Status::Accepted
                    }
                    AdapterResult::Dropped(reason) => Status::Queued { hold: Some(reason) },
                    AdapterResult::Uncertain => Status::Uncertain,
                    AdapterResult::Failed(reason) => Status::Failed { reason },
                };
            }
            self.trim_history(&mut next);
            self.update(next, &completion.id)?;
        }
        Ok(())
    }

    /// Only an uncertain message with the same binding and content hash can be
    /// settled, and only to accepted or failed: nothing is ever resent. A row
    /// already settled with the same outcome is re-saved and reported Ok, so a
    /// resolve whose directory sync failed after the rename can replay and
    /// the caller drops its evidence only after a successful save.
    pub fn resolve(
        &mut self,
        id: &str,
        binding: &Binding,
        hash: &str,
        outcome: Resolution,
    ) -> io::Result<()> {
        let mut next = self.state.clone();
        let item = next
            .messages
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "message not found"))?;
        if &item.binding != binding || item.hash != hash {
            return Err(io::Error::other(
                "evidence does not match an uncertain message",
            ));
        }
        let same_outcome = match (&item.status, &outcome) {
            (Status::Accepted, Resolution::Accepted(receipt)) => item.receipt == *receipt,
            (Status::Failed { reason }, Resolution::Failed(same)) => reason == same,
            _ => false,
        };
        if same_outcome {
            return self.update(self.state.clone(), id);
        }
        if item.status != Status::Uncertain {
            return Err(io::Error::other(
                "evidence does not match an uncertain message",
            ));
        }
        item.status = match outcome {
            Resolution::Accepted(receipt) => {
                item.receipt = receipt;
                Status::Accepted
            }
            Resolution::Failed(reason) => Status::Failed { reason },
        };
        self.trim_history(&mut next);
        self.update(next, id)
    }

    pub fn acknowledge(&mut self, id: &str, binding: &Binding, hash: &str) -> io::Result<()> {
        let mut next = self.state.clone();
        let item = next
            .messages
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "message not found"))?;
        if &item.binding != binding || item.hash != hash || item.status != Status::Accepted {
            return Err(io::Error::other("receipt does not match accepted message"));
        }
        item.status = Status::Acknowledged;
        self.trim_history(&mut next);
        self.update(next, id)
    }
}
