//! Routes the agent text of native OpenCode rooms through the durable outbox
//! and the OpenCode adapter, and reports each message's status. Nothing here
//! writes to a terminal: a native room never receives paste.

use std::{
    collections::{HashMap, VecDeque},
    io,
    path::PathBuf,
        sync::{
        Arc, Mutex,
        mpsc::{Receiver, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    mailroom::Mailroom,
    native_launch::Reaped,
    native_outbox::{
        Adapter, AdapterResult, BackgroundOutbox, Binding, Command, Event, Message, Resolution,
        Status,
    },
    opencode_native::{Delivery, Server},
    opencode_outbox::{OpenCodeAdapter, native_id},
    pane::{session_state::create_private_directory, whisper_note},
};

const OUTBOX_CAPACITY: usize = 200;
const OUTBOX_HISTORY: usize = 100;
const MAX_IN_FLIGHT: usize = 4;
const SCHEDULE_INTERVAL: Duration = Duration::from_millis(100);
/// How long a caller waits for a message to leave the queue before being told
/// it is only saved. The wait is spent in the event loop's own ticks. The
/// doorbell gives up on an answer after 2 seconds, so this must stay clearly
/// below that or a saved message would be reported as a timeout.
const REPLY_WAIT: Duration = Duration::from_millis(1500);
const STARTUP_WAIT: Duration = Duration::from_secs(5);
/// An uncertain message is checked against the native inbox this often.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Hands messages to whichever adapter is current, so rooms can be bound again
/// without restarting the outbox thread that holds this object.
pub(crate) struct SwitchAdapter {
    current: Mutex<Arc<OpenCodeAdapter>>,
}

impl SwitchAdapter {
    pub(crate) fn new(rooms: Vec<(Binding, Arc<Server>)>) -> io::Result<Self> {
        Ok(Self {
            current: Mutex::new(Arc::new(OpenCodeAdapter::new(rooms)?)),
        })
    }

    /// Replaces the set of bound rooms. A message for a room that is no longer
    /// in the set is held by the adapter, never sent to another room. Crowded
    /// relaunches a native room only by restarting, which builds a new router,
    /// so nothing calls this yet outside the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rebind(&self, rooms: Vec<(Binding, Arc<Server>)>) -> io::Result<()> {
        let next = Arc::new(OpenCodeAdapter::new(rooms)?);
        let previous = {
            let mut current = self
                .current
                .lock()
                .map_err(|_| io::Error::other("native adapter state is poisoned"))?;
            std::mem::replace(&mut *current, next)
        };
        // The old adapter may hold the last handle to a server, whose drop
        // waits for the child; that happens on the reaper thread.
        drop(Reaped::new(previous));
        Ok(())
    }
}

impl Adapter for SwitchAdapter {
    fn send(&self, message: Message) -> AdapterResult {
        let adapter = match self.current.lock() {
            Ok(current) => Arc::clone(&current),
            Err(_) => return AdapterResult::Dropped("native routing is unavailable".into()),
        };
        adapter.send(message)
    }
}

/// What a caller is told about a message it handed over.
#[derive(Debug, PartialEq)]
pub(crate) enum NativeReply {
    /// OpenCode accepted it into its queue. Not proof the agent read it.
    Accepted,
    /// Saved here; still waiting its turn or for delivery to resume.
    Queued,
    /// Saved but not sent, and it needs attention.
    Held(String),
    /// It may have been accepted. It is never resent.
    Uncertain,
    Failed(String),
}

/// Called once with the mailroom envelope number and the outcome.
pub(crate) type Reply = Box<dyn FnOnce(u64, NativeReply)>;

/// One native room, as the router needs it.
pub(crate) struct RoomEntry {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) binding: Binding,
    pub(crate) server: Arc<Server>,
}

enum Waiting {
    Enqueue { envelope: u64, reply: Option<Reply> },
    Schedule,
    Resolve,
}

struct PendingReply {
    outbox_id: String,
    envelope: u64,
    reply: Reply,
    deadline: Instant,
}

pub(crate) struct NativeRouter {
    /// Taken in `drop` so the outbox thread can finish before servers are released.
    outbox: Option<BackgroundOutbox>,
    #[cfg_attr(not(test), allow(dead_code))]
    adapter: Reaped<Arc<SwitchAdapter>>,
    path: PathBuf,
    rooms: HashMap<usize, (String, Binding)>,
        bindings: HashMap<String, Binding>,
        /// Held in a [`Reaped`] so the last handle to a server is never dropped
    /// on the event loop.
    servers: Reaped<HashMap<String, Arc<Server>>>,
    titles: HashMap<String, String>,
    reconciling: HashMap<String, Receiver<Delivery>>,
    last_reconcile: HashMap<String, Instant>,
    waiting: VecDeque<Waiting>,
    schedule_in_flight: bool,
    last_schedule: Option<Instant>,
    pending: Vec<PendingReply>,
    envelope_of: HashMap<String, u64>,
    shown: HashMap<String, String>,
    messages: Vec<Message>,
    paused: bool,
    unavailable: Option<String>,
}

/// How long closing waits for the outbox thread and any send still in flight to
/// let go of the servers.
const CLOSE_WAIT: Duration = Duration::from_secs(10);

impl Drop for NativeRouter {
    /// Servers are killed when the last handle to them is dropped, and the
    /// outbox thread and its in-flight sends hold handles through the adapter.
    /// Closing the outbox and waiting for those to let go makes the kill happen
    /// before the process exits, however it was asked to exit.
    fn drop(&mut self) {
        drop(self.outbox.take());
        let deadline = Instant::now() + CLOSE_WAIT;
        while Arc::strong_count(self.adapter.get()) > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl NativeRouter {
    fn outbox(&self) -> &BackgroundOutbox {
        self.outbox.as_ref().expect("the outbox is open until the router is dropped")
    }

    /// Opens the outbox at `path` (creating its private directory) and binds
    /// the rooms. State from an earlier run is loaded and listed; a corrupt or
    /// locked file is an error and is left as it is.
    pub(crate) fn start(path: PathBuf, rooms: Vec<RoomEntry>) -> io::Result<Self> {
                if let Some(parent) = path.parent() {
            if parent.exists() && !parent.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists but is not a directory; native delivery keeps its saved mail there",
                        parent.display()
                    ),
                ));
            }
            create_private_directory(parent).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot create {} for native delivery: {error}", parent.display()),
                )
            })?;
        }
        let adapter = Arc::new(SwitchAdapter::new(
            rooms
                .iter()
                .map(|room| (room.binding.clone(), Arc::clone(&room.server)))
                .collect(),
        )?);
        let outbox = BackgroundOutbox::start(
            path.clone(),
            OUTBOX_CAPACITY,
            OUTBOX_HISTORY,
            MAX_IN_FLIGHT,
            Arc::clone(&adapter) as Arc<dyn Adapter>,
        );
                let mut bindings = HashMap::new();
        let mut servers = HashMap::new();
        let mut titles = HashMap::new();
        let mut by_index = HashMap::new();
        for room in &rooms {
            bindings.insert(room.binding.room.clone(), room.binding.clone());
            servers.insert(room.binding.room.clone(), Arc::clone(&room.server));
            titles.insert(room.binding.room.clone(), room.title.clone());
            by_index.insert(room.index, (room.title.clone(), room.binding.clone()));
        }
        let mut router = Self {
            outbox: Some(outbox),
            adapter: Reaped::new(adapter),
            path,
            rooms: by_index,
                                    bindings,
            servers: Reaped::new(servers),
            titles,
            reconciling: HashMap::new(),
            last_reconcile: HashMap::new(),
            waiting: VecDeque::new(),
            schedule_in_flight: false,
            last_schedule: None,
            pending: Vec::new(),
            envelope_of: HashMap::new(),
            shown: HashMap::new(),
            messages: Vec::new(),
            paused: true,
            unavailable: None,
        };
        // A paused schedule loads the saved state without sending anything, and
        // tells us at once whether the file could be opened.
        router.send_schedule(true, false)?;
        match router.outbox().events.recv_timeout(STARTUP_WAIT) {
            Ok(Event::Updated(messages)) => {
                router.waiting.pop_front();
                router.schedule_in_flight = false;
                router.messages = messages;
                Ok(router)
            }
            Ok(Event::Error(error)) => Err(io::Error::other(format!(
                "native outbox {} could not be opened and was left unchanged: {error}",
                router.path.display()
            ))),
            _ => Err(io::Error::other(format!(
                "native outbox {} did not start in time",
                router.path.display()
            ))),
        }
    }

    pub(crate) fn is_native(&self, index: usize) -> bool {
        self.rooms.contains_key(&index)
    }

    /// Replaces the bound rooms (a new launch generation). Mail for a binding
    /// that is no longer current stays in the outbox as held; it is never
    /// moved to the new binding.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rebind(&mut self, rooms: Vec<RoomEntry>) -> io::Result<()> {
        self.adapter.get().rebind(
            rooms
                .iter()
                .map(|room| (room.binding.clone(), Arc::clone(&room.server)))
                .collect(),
        )?;
                        self.bindings.clear();
        let mut servers = HashMap::new();
        self.rooms.clear();
        for room in &rooms {
            self.bindings
                .insert(room.binding.room.clone(), room.binding.clone());
                        servers.insert(room.binding.room.clone(), Arc::clone(&room.server));
            self.titles
                .insert(room.binding.room.clone(), room.title.clone());
                        self.rooms
                .insert(room.index, (room.title.clone(), room.binding.clone()));
        }
        // The replaced map may hold the last handle to an old server.
        drop(std::mem::replace(&mut self.servers, Reaped::new(servers)));
        Ok(())
    }

    /// Hands a whisper to a native room: saved first, then sent by the outbox.
    /// `reply` hears how far it got; an `Err` means nothing was saved.
    pub(crate) fn send(
        &mut self,
        mailroom: &mut Mailroom,
        from: &str,
        to: usize,
        message: &str,
        reply: Option<Reply>,
    ) -> Result<(), String> {
                if let Some(error) = &self.unavailable {
            let error = format!("native delivery is unavailable: {error}");
            if let Some(reply) = reply {
                reply(0, NativeReply::Failed(error.clone()));
            }
            return Err(error);
        }
        let Some((title, binding)) = self.rooms.get(&to) else {
            let error = "that room does not use native delivery".to_owned();
            if let Some(reply) = reply {
                reply(0, NativeReply::Failed(error.clone()));
            }
            return Err(error);
        };
        let (title, binding) = (title.clone(), binding.clone());
        let envelope = mailroom.record_native(
            from.to_owned(),
            title,
            message.to_owned(),
            "SAVING",
        );
        let command = Command::Enqueue {
            source: from.to_owned(),
            binding,
            text: whisper_note(from, message),
        };
        match self.outbox().try_send(command) {
            Ok(()) => {
                self.waiting.push_back(Waiting::Enqueue { envelope, reply });
                Ok(())
            }
                        Err(TrySendError::Full(_)) => {
                let error = "native delivery is busy; try again".to_owned();
                mailroom.set_native_status(envelope, format!("FAILED: {error}"));
                if let Some(reply) = reply {
                    reply(envelope, NativeReply::Failed(error.clone()));
                }
                Err(error)
            }
            Err(TrySendError::Disconnected(_)) => {
                let error = "the native outbox stopped".to_owned();
                self.unavailable = Some(error.clone());
                mailroom.set_native_status(envelope, format!("FAILED: {error}"));
                let error = format!("native delivery is unavailable: {error}");
                if let Some(reply) = reply {
                    reply(envelope, NativeReply::Failed(error.clone()));
                }
                Err(error)
            }
        }
    }

    /// Advances the outbox and collects what it reports. Call it every pass of
    /// the event loop; it never blocks. Returns short notices worth showing.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        mailroom: &mut Mailroom,
        paused: bool,
        fused: bool,
    ) -> Vec<String> {
        let mut notices = self.drain(now, mailroom);
        self.paused = paused || fused;
        if self.unavailable.is_none()
            && !self.schedule_in_flight
            && self
                .last_schedule
                .is_none_or(|last| now.duration_since(last) >= SCHEDULE_INTERVAL)
        {
            if let Err(error) = self.send_schedule(paused, fused) {
                notices.push(error.to_string());
            }
            self.last_schedule = Some(now);
        }
                self.resolve_pending(now);
        self.reconcile_uncertain(now);
        notices
    }

    /// An uncertain message may already be in OpenCode's queue. Look for it
    /// there with a read-only request that cannot send anything; if the exact
    /// message is found it is accepted, if different content holds its ID it
    /// fails, and otherwise it stays uncertain and is never resent.
    fn reconcile_uncertain(&mut self, now: Instant) {
        let mut finished = Vec::new();
        for (id, receiver) in &self.reconciling {
            match receiver.try_recv() {
                Ok(outcome) => finished.push((id.clone(), Some(outcome))),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => finished.push((id.clone(), None)),
            }
        }
        for (id, outcome) in finished {
            self.reconciling.remove(&id);
            let Some(message) = self.messages.iter().find(|m| m.id == id) else {
                continue;
            };
            let resolution = match outcome {
                Some(Delivery::Accepted) => Resolution::Accepted(native_id(&id)),
                Some(Delivery::Conflict) => Resolution::Failed(
                    "OpenCode holds different content under this message ID".to_owned(),
                ),
                _ => continue,
            };
            let command = Command::Resolve {
                id,
                binding: message.binding.clone(),
                hash: message.hash.clone(),
                outcome: resolution,
            };
            if self.outbox().try_send(command).is_ok() {
                self.waiting.push_back(Waiting::Resolve);
            }
        }
        for message in &self.messages {
            if message.status != Status::Uncertain
                || self.reconciling.contains_key(&message.id)
                || self.bindings.get(&message.destination) != Some(&message.binding)
                || self
                    .last_reconcile
                    .get(&message.id)
                    .is_some_and(|last| now.duration_since(*last) < RECONCILE_INTERVAL)
            {
                continue;
            }
            let (Some(server), Some(native)) = (
                self.servers.get().get(&message.destination),
                native_id(&message.id),
            ) else {
                continue;
            };
            self.last_reconcile.insert(message.id.clone(), now);
            self.reconciling.insert(
                message.id.clone(),
                server.reconcile_async(&message.binding, native, message.text.clone()),
            );
        }
    }

    fn send_schedule(&mut self, paused: bool, fused: bool) -> io::Result<()> {
        let command = Command::Schedule {
            bindings: self.bindings.clone(),
            paused,
            fused,
        };
        match self.outbox().try_send(command) {
            Ok(()) => {
                self.waiting.push_back(Waiting::Schedule);
                self.schedule_in_flight = true;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                let error = "the native outbox stopped".to_owned();
                self.unavailable = Some(error.clone());
                Err(io::Error::other(error))
            }
        }
    }

    fn drain(&mut self, now: Instant, mailroom: &mut Mailroom) -> Vec<String> {
        let mut notices = Vec::new();
        while let Ok(event) = self.outbox().events.try_recv() {
            match event {
                Event::Queued(outbox_id) => {
                    if let Some(Waiting::Enqueue { envelope, reply }) = self.waiting.pop_front() {
                        self.envelope_of.insert(outbox_id.clone(), envelope);
                        mailroom.set_native_status(envelope, "QUEUED: saved, not yet sent");
                        if let Some(reply) = reply {
                            self.pending.push(PendingReply {
                                outbox_id,
                                envelope,
                                reply,
                                deadline: now + REPLY_WAIT,
                            });
                        }
                    }
                }
                Event::Updated(messages) => {
                                        match self.waiting.front() {
                        Some(Waiting::Schedule) => {
                            self.waiting.pop_front();
                            self.schedule_in_flight = false;
                        }
                        Some(Waiting::Resolve) => {
                            self.waiting.pop_front();
                        }
                        _ => {}
                    }
                    self.messages = messages;
                    self.sync_mailroom(mailroom);
                }
                Event::Error(error) => match self.waiting.pop_front() {
                    Some(Waiting::Enqueue { envelope, reply }) => {
                        mailroom.set_native_status(envelope, format!("FAILED: {error}"));
                        if let Some(reply) = reply {
                            reply(envelope, NativeReply::Failed(error));
                        }
                    }
                                        Some(Waiting::Schedule) => {
                        self.schedule_in_flight = false;
                        notices.push(format!("native delivery: {error}"));
                    }
                    Some(Waiting::Resolve) => {
                        notices.push(format!("native delivery: {error}"));
                    }
                    None => {
                        // The outbox reports a failed open with no command behind it.
                        self.unavailable = Some(error.clone());
                        notices.push(format!("native delivery stopped: {error}"));
                    }
                },
            }
        }
        notices
    }

    fn status_text(&self, message: &Message) -> String {
        let current = self.bindings.get(&message.destination) == Some(&message.binding);
        match &message.status {
            Status::Queued { hold: None } if !current => {
                "HELD: sent for an earlier launch of this room; not delivered".to_owned()
            }
            Status::Queued { hold: None } if self.paused => {
                "QUEUED: delivery is paused".to_owned()
            }
            Status::Queued { hold: None } => "QUEUED".to_owned(),
            Status::Queued { hold: Some(reason) } => format!("HELD: {reason}"),
            Status::Sending => "SENDING".to_owned(),
            Status::Accepted => {
                "ACCEPTED by OpenCode (queued there; not proof it was read)".to_owned()
            }
            Status::Acknowledged => "ACKNOWLEDGED".to_owned(),
            Status::Uncertain => {
                "UNCERTAIN: may have been accepted; it will not be resent".to_owned()
            }
            Status::Failed { reason } => format!("FAILED: {reason}"),
        }
    }

    fn label(&self, message: &Message) -> String {
        match self.titles.get(&message.destination) {
            Some(title) => title.clone(),
            None => format!(
                "room {} (earlier launch)",
                message.destination.split('.').next().unwrap_or("?")
            ),
        }
    }

    fn sync_mailroom(&mut self, mailroom: &mut Mailroom) {
        let messages = std::mem::take(&mut self.messages);
        for message in &messages {
            let text = self.status_text(message);
            if self.shown.get(&message.id) == Some(&text) {
                continue;
            }
            match self.envelope_of.get(&message.id) {
                Some(&envelope) => mailroom.set_native_status(envelope, text.clone()),
                None => {
                    // Saved by an earlier run of Crowded.
                    let envelope = mailroom.record_native(
                        message.source.clone(),
                        self.label(message),
                        message.text.clone(),
                        text.clone(),
                    );
                    self.envelope_of.insert(message.id.clone(), envelope);
                }
            }
            self.shown.insert(message.id.clone(), text);
        }
        self.messages = messages;
    }

    fn resolve_pending(&mut self, now: Instant) {
        let pending = std::mem::take(&mut self.pending);
        for entry in pending {
            let message = self.messages.iter().find(|m| m.id == entry.outbox_id);
            let outcome = match message.map(|m| &m.status) {
                Some(Status::Accepted | Status::Acknowledged) => Some(NativeReply::Accepted),
                Some(Status::Uncertain) => Some(NativeReply::Uncertain),
                Some(Status::Failed { reason }) => Some(NativeReply::Failed(reason.clone())),
                Some(Status::Queued { hold: Some(reason) }) => {
                    Some(NativeReply::Held(reason.clone()))
                }
                                // Not in the latest snapshot yet, or still waiting its turn.
                None | Some(Status::Queued { hold: None } | Status::Sending) => {
                    (now >= entry.deadline).then_some(NativeReply::Queued)
                }
            };
            match outcome {
                Some(outcome) => (entry.reply)(entry.envelope, outcome),
                None => self.pending.push(entry),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::native_launch::{
        self, NativeRoom,
        tests::{FakeMode, FakeRoom, fake_room},
    };
    use std::{cell::RefCell, fs, path::Path, rc::Rc, sync::atomic::Ordering, thread};

    struct Launched {
        room: NativeRoom,
        fake: FakeRoom,
    }

    fn launch(root: &Path, cwd: &Path, name: &str, number: usize, generation: u64) -> Launched {
        let fake = fake_room(root, name, "v2.2.5");
        let room = native_launch::start(native_launch::NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd,
            format!("{number}.{generation}"),
            generation,
            &[],
        ))
        .unwrap();
        Launched { room, fake }
    }

    fn entry(launched: &Launched, index: usize, title: &str) -> RoomEntry {
        RoomEntry {
            index,
            title: title.to_owned(),
            binding: launched.room.binding().clone(),
            server: launched.room.server(),
        }
    }

    type Log = Rc<RefCell<Vec<(u64, NativeReply)>>>;

    fn recorder(log: &Log) -> Option<Reply> {
        let log = Rc::clone(log);
        Some(Box::new(move |id, outcome| log.borrow_mut().push((id, outcome))))
    }

    fn pump(
        router: &mut NativeRouter,
        mailroom: &mut Mailroom,
        paused: bool,
        fused: bool,
        done: impl Fn(&Mailroom) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.tick(Instant::now(), mailroom, paused, fused);
            if done(mailroom) {
                return;
            }
            assert!(Instant::now() < deadline, "never finished:\n{}", mailroom.summary());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn pump_for(router: &mut NativeRouter, mailroom: &mut Mailroom, paused: bool, fused: bool, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            router.tick(Instant::now(), mailroom, paused, fused);
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn state_path(root: &Path) -> PathBuf {
        root.join("state").join("native-outbox.json")
    }

    /// The previous router's outbox thread releases its lock asynchronously.
    fn start_again(path: &Path, rooms: impl Fn() -> Vec<RoomEntry>) -> NativeRouter {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match NativeRouter::start(path.to_path_buf(), rooms()) {
                Ok(router) => return router,
                Err(error) => assert!(Instant::now() < deadline, "{error}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn texts(launched: &Launched) -> Vec<String> {
        launched
            .fake
            .state
            .received
            .lock()
            .unwrap()
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }

    #[test]
    fn a_whisper_is_saved_then_accepted_exactly_and_the_other_room_gets_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let mut router = NativeRouter::start(
            state_path(root.path()),
            vec![entry(&a, 0, "Room A"), entry(&b, 1, "Room B")],
        )
        .unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router
            .send(&mut mailroom, "Room B", 0, "multi\nline \u{2713} \u{fc}\u{f1}\u{ed}", recorder(&log))
            .unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());

        assert_eq!(log.borrow()[0].1, NativeReply::Accepted);
        let received = a.fake.state.received.lock().unwrap().clone();
        assert_eq!(received.len(), 1);
        assert!(received[0].0.starts_with("msg_") && received[0].0.len() == 68);
        assert_eq!(
            received[0].1,
            "[whisper from Room B] multi\nline \u{2713} \u{fc}\u{f1}\u{ed}"
        );
        assert!(texts(&b).is_empty());
        // A delivered message is meant to be acted on, not only stored.
        assert_eq!(*a.fake.state.resumes.lock().unwrap(), [true]);
        let view = mailroom.summary();
        assert!(view.contains("ACCEPTED by OpenCode"), "{view}");
        assert!(view.contains("Room B \u{2192} Room A"), "{view}");
    }

    #[test]
    fn mail_to_a_busy_room_waits_its_turn_and_arrives_in_order() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        a.fake.state.delay_ms.store(250, Ordering::SeqCst);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        for text in ["one", "two", "three"] {
            router
                .send(&mut mailroom, "Room B", 0, text, recorder(&log))
                .unwrap();
        }
        // While the first is in flight the others are saved and waiting.
        pump(&mut router, &mut mailroom, false, false, |view| {
            let view = view.summary();
            view.contains("SENDING") && view.contains("QUEUED")
        });
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 3);
        assert!(log.borrow().iter().all(|(_, outcome)| *outcome == NativeReply::Accepted));
        let order: Vec<String> = texts(&a);
        assert_eq!(
            order,
            [
                "[whisper from Room B] one",
                "[whisper from Room B] two",
                "[whisper from Room B] three"
            ]
        );
    }

    #[test]
    fn a_restart_keeps_uncertain_and_waiting_mail_and_never_resends_or_retargets_it() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = state_path(root.path());
        {
            let first = launch(root.path(), cwd.path(), "first", 1, 1);
            *first.fake.state.mode.lock().unwrap() = FakeMode::Drop;
            let mut router = NativeRouter::start(path.clone(), vec![entry(&first, 0, "Room A")]).unwrap();
            let mut mailroom = Mailroom::new(50);
            let log = Log::default();
            router.send(&mut mailroom, "Room B", 0, "lost reply", recorder(&log)).unwrap();
            pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
            assert_eq!(log.borrow()[0].1, NativeReply::Uncertain);
            // A second message is saved while delivery is paused.
            router.send(&mut mailroom, "Room B", 0, "waiting", None).unwrap();
            pump_for(&mut router, &mut mailroom, true, false, 400);
            let view = mailroom.summary();
            assert!(view.contains("UNCERTAIN"), "{view}");
            assert!(view.contains("QUEUED: delivery is paused"), "{view}");
            assert!(texts(&first).is_empty());
        }
        native_launch::wait_for_reaps();

        // Crowded starts again: a new server, a new session, a new generation.
        let second = launch(root.path(), cwd.path(), "second", 1, 2);
        let mut router = start_again(&path, || vec![entry(&second, 0, "Room A")]);
        let mut mailroom = Mailroom::new(50);
        pump(&mut router, &mut mailroom, false, false, |view| {
            let view = view.summary();
            view.contains("UNCERTAIN") && view.contains("HELD: sent for an earlier launch")
        });
        let view = mailroom.summary();
        assert!(view.contains("room 1 (earlier launch)"), "{view}");

        // New mail reaches the new launch; the old mail neither follows nor repeats.
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "fresh", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(log.borrow()[0].1, NativeReply::Accepted);
        pump_for(&mut router, &mut mailroom, false, false, 400);
        assert_eq!(texts(&second), ["[whisper from Room B] fresh"]);
    }

    #[test]
    fn a_rejected_login_fails_the_message_with_a_short_reason() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        *a.fake.state.mode.lock().unwrap() = FakeMode::Unauthorized;
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "hello", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        match &log.borrow()[0].1 {
            NativeReply::Failed(reason) => assert!(reason.contains("401"), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(mailroom.summary().contains("FAILED: OpenCode returned HTTP 401"));
        assert!(texts(&a).is_empty());
    }

    #[test]
    fn a_lost_reply_is_found_in_the_native_inbox_and_the_room_carries_on() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();

        // OpenCode stores the message, then the reply is lost.
        *a.fake.state.mode.lock().unwrap() = FakeMode::StoreThenDrop;
        router.send(&mut mailroom, "Room B", 0, "kept", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 1);
        assert_eq!(log.borrow()[0].1, NativeReply::Uncertain);

        // A read-only look at the inbox settles it without sending anything again.
        *a.fake.state.mode.lock().unwrap() = FakeMode::Normal;
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("ACCEPTED by OpenCode")
        });
        assert_eq!(texts(&a), ["[whisper from Room B] kept"]);

        // The room's lane is free again.
        router.send(&mut mailroom, "Room B", 0, "next", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 2);
        assert_eq!(log.borrow()[1].1, NativeReply::Accepted);
        assert_eq!(
            texts(&a),
            ["[whisper from Room B] kept", "[whisper from Room B] next"]
        );
    }

    #[test]
    fn a_lost_reply_with_no_trace_stays_uncertain_and_later_mail_waits_visibly() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        *a.fake.state.mode.lock().unwrap() = FakeMode::Drop;
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "lost", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 1);
        assert_eq!(log.borrow()[0].1, NativeReply::Uncertain);

        *a.fake.state.mode.lock().unwrap() = FakeMode::Normal;
        router.send(&mut mailroom, "Room B", 0, "behind it", recorder(&log)).unwrap();
        pump_for(&mut router, &mut mailroom, false, false, 600);
        // Nothing was resent, and the message behind it was not sent past it.
        assert!(texts(&a).is_empty());
        let view = mailroom.summary();
        assert!(view.contains("UNCERTAIN: may have been accepted; it will not be resent"), "{view}");
        assert!(view.contains("QUEUED"), "{view}");
    }

    #[test]
    fn a_dead_server_holds_the_mail_instead_of_losing_or_pasting_it() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        a.fake.shutdown();
        router.send(&mut mailroom, "Room B", 0, "dead", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 1);
        assert!(
            matches!(&log.borrow()[0].1, NativeReply::Held(reason) if reason.contains("not sent")),
            "{:?}",
            log.borrow()[0].1
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: OpenCode request was not sent"), "{view}");
        assert!(!view.contains("ACCEPTED"), "{view}");
    }

    #[test]
    fn pause_and_fuse_keep_mail_queued_until_delivery_resumes() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "later", recorder(&log)).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 400);
        pump_for(&mut router, &mut mailroom, false, true, 400);
        assert!(texts(&a).is_empty());
        assert!(mailroom.summary().contains("QUEUED"));
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(texts(&a), ["[whisper from Room B] later"]);
    }

    #[test]
    fn closing_the_router_releases_the_servers_even_with_a_send_in_flight() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        a.fake.state.delay_ms.store(500, Ordering::SeqCst);
        let pid = fs::read_to_string(a.fake.dir.join("pid")).unwrap();
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        router.send(&mut mailroom, "Room B", 0, "in flight", None).unwrap();
        pump(&mut router, &mut mailroom, false, false, |view| view.summary().contains("SENDING"));
        // Exit while the send is still being answered. Closing waits for the
        // send to finish, not for the full close timeout.
        let closing = Instant::now();
        drop(router);
        assert!(closing.elapsed() < Duration::from_secs(5), "closing hung");
        drop(a.room);
        native_launch::wait_for_reaps();
        assert!(
            !native_launch::tests::process_alive(&pid),
            "the server must be gone once the reap wait returns"
        );
    }

    #[test]
    fn the_reply_wait_ends_before_the_doorbell_gives_up_on_an_answer() {
        // server_unix.rs and server_windows.rs wait 2 seconds for the app's answer.
        assert!(REPLY_WAIT <= Duration::from_millis(1500));
    }

    #[test]
    fn a_message_that_cannot_leave_yet_is_answered_as_queued_not_delivered() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "later", recorder(&log)).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 600);
        assert!(log.borrow().is_empty(), "still within the wait");
        pump_for(&mut router, &mut mailroom, true, false, 1500);
        assert_eq!(log.borrow().len(), 1);
        assert_eq!(log.borrow()[0].1, NativeReply::Queued);
        assert!(texts(&a).is_empty());
    }

    #[test]
    fn a_corrupt_outbox_is_reported_and_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let path = state_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        let error = NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")])
            .err()
            .expect("must refuse");
        assert!(error.to_string().contains("left unchanged"), "{error}");
        assert!(error.to_string().contains("native-outbox.json"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
    }

    #[test]
    fn a_state_directory_that_is_a_file_is_explained_not_reported_as_file_exists() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let path = state_path(root.path());
        fs::write(path.parent().unwrap(), b"in the way").unwrap();
        let error = NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")])
            .err()
            .expect("must refuse");
        let message = error.to_string();
        assert!(message.contains("not a directory"), "{message}");
        assert!(message.contains(path.parent().unwrap().to_str().unwrap()), "{message}");
        assert!(!message.contains("os error"), "{message}");
        assert_eq!(fs::read(path.parent().unwrap()).unwrap(), b"in the way");
    }

    #[test]
    fn a_state_directory_that_cannot_be_created_is_named_in_the_error() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let locked = root.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let path = locked.join("state").join("native-outbox.json");
        let result = NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let Err(error) = result else {
            // Running with enough privilege to ignore the permissions.
            return;
        };
        let message = error.to_string();
        assert!(message.contains(path.parent().unwrap().to_str().unwrap()), "{message}");
        assert!(message.contains("cannot create"), "{message}");
    }

    #[test]
    fn the_outbox_directory_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let path = state_path(root.path());
        let mut router = NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "hi", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn rebinding_to_a_new_launch_holds_old_mail_and_delivers_new_mail_to_the_new_server() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let old = launch(root.path(), cwd.path(), "old", 1, 1);
        let new = launch(root.path(), cwd.path(), "new", 1, 2);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&old, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        router.send(&mut mailroom, "Room B", 0, "for the old launch", None).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 300);

        router.rebind(vec![entry(&new, 0, "Room A")]).unwrap();
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "for the new launch", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        pump_for(&mut router, &mut mailroom, false, false, 300);

        assert_eq!(texts(&new), ["[whisper from Room B] for the new launch"]);
        assert!(texts(&old).is_empty());
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    #[test]
    fn the_switching_adapter_sends_nothing_for_a_room_it_does_not_know() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let adapter = SwitchAdapter::new(vec![(a.room.binding().clone(), a.room.server())]).unwrap();
        let stranger = Binding {
            room: "9.9".to_owned(),
            session: "ses_x".to_owned(),
            endpoint: "http://127.0.0.1:1".to_owned(),
            generation: 9,
        };
        let text = "hello".to_owned();
        let message = Message {
            id: "a".repeat(64),
            source: "s".to_owned(),
            destination: stranger.room.clone(),
            binding: stranger,
            hash: format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())),
            text,
            created_ms: 0,
            sequence: 1,
            attempts: Vec::new(),
            receipt: None,
            status: Status::Sending,
        };
        assert!(matches!(adapter.send(message), AdapterResult::Dropped(_)));
        assert!(texts(&a).is_empty());
    }
}
