//! Routes the agent text of native OpenCode rooms through the durable outbox
//! and the OpenCode adapter, and reports each message's status. Nothing here
//! writes to a terminal: a native room never receives paste.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
        sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    claude_inbox::ClaudeInbox,
    codex_native::{CodexAdapter, Room},
    mailroom::Mailroom,
    native_launch::Reaped,
    native_outbox::{
        Adapter, AdapterResult, BackgroundOutbox, Binding, Command, Event, Message, Resolution,
        Status, TaggedEvent,
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
    current: Mutex<Arc<Routing>>,
}

/// The per-vendor adapters of one binding set. A room is sent through the
/// adapter that owns it, and an unknown room is held by both.
struct Routing {
    opencode: OpenCodeAdapter,
    codex: CodexAdapter,
    /// Claude rooms and the inbox their mail is delivered through.
    claude: HashMap<String, Arc<ClaudeInbox>>,
}

impl Routing {
    fn new(rooms: Vec<RoomEntry>) -> io::Result<Self> {
        let mut opencode = Vec::new();
        let mut codex = Vec::new();
        let mut claude = HashMap::new();
        for room in rooms {
            match room.backend {
                Backend::OpenCode(server) => opencode.push((room.binding, server)),
                Backend::Codex(native) => codex.push((room.binding, (*native).clone())),
                Backend::Claude(inbox) => {
                    claude.insert(room.binding.room.clone(), inbox);
                }
            }
        }
        Ok(Self {
            opencode: OpenCodeAdapter::new(opencode)?,
            codex: CodexAdapter::new(codex)?,
            claude,
        })
    }

    fn send(&self, message: Message) -> AdapterResult {
        if let Some(inbox) = self.claude.get(&message.destination) {
            return inbox.deliver(&message);
        }
        if self.codex.has_room(&message.destination) {
            self.codex.send(message)
        } else {
            self.opencode.send(message)
        }
    }
}

impl SwitchAdapter {
    pub(crate) fn new(rooms: Vec<RoomEntry>) -> io::Result<Self> {
        Ok(Self {
            current: Mutex::new(Arc::new(Routing::new(rooms)?)),
        })
    }

    /// Replaces the set of bound rooms. A message for a room that is no longer
    /// in the set is held by the adapter, never sent to another room. Crowded
    /// relaunches a native room only by restarting, which builds a new router,
    /// so nothing calls this yet outside the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rebind(&self, rooms: Vec<RoomEntry>) -> io::Result<()> {
        let next = Arc::new(Routing::new(rooms)?);
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
#[derive(Clone)]
pub(crate) struct RoomEntry {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) binding: Binding,
    pub(crate) backend: Backend,
}

/// How one room accepts native mail.
#[derive(Clone)]
pub(crate) enum Backend {
    /// Crowded owns an OpenCode server and prompts it over its API.
    OpenCode(Arc<Server>),
    /// The room's own Codex process is queued through the CLI.
    Codex(Arc<Room>),
    /// The room's own Claude process, reached through the loopback inbox its
    /// mod polls.
    Claude(Arc<ClaudeInbox>),
}

/// A Codex room handed to the router. Without a verified thread the room is
/// still native: mail is held and explained instead of pasted.
pub(crate) struct CodexEntry {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) room: Option<Arc<Room>>,
    /// The launch generation this thread belongs to; it never repeats, so mail
    /// saved for an earlier launch can never match a later binding.
    pub(crate) generation: u64,
    /// Why the room has no session, when it has none.
    pub(crate) reason: Option<String>,
}

const UNBOUND_CODEX: &str =
    "this room has no Codex session; restart Crowded and send again";

/// A Claude room handed to the router. Crowded chose the session id itself, so
/// the room is bound as soon as it starts; a room that could not start carries
/// the reason instead.
pub(crate) struct ClaudeEntry {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) session: Option<String>,
    /// The launch generation this session belongs to; it never repeats, so mail
    /// saved for an earlier launch can never match a later binding.
    pub(crate) generation: u64,
    /// Why the room has no session, when it has none.
    pub(crate) reason: Option<String>,
}

const UNBOUND_CLAUDE: &str = "no Claude mod connected";

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
    /// Destination keys of the Codex rooms, for vendor-specific status text.
    codex: HashSet<String>,
    /// The bound Codex rooms, kept whole so every rebuild of the adapter's
    /// routing table includes the ones bound earlier.
    codex_rooms: HashMap<usize, RoomEntry>,
    /// Destination keys of the Claude rooms, for vendor-specific status text.
    claude: HashSet<String>,
    /// The bound Claude rooms, kept whole for the same reason as the Codex ones.
    claude_rooms: HashMap<usize, RoomEntry>,
    /// Each Claude room's inbox, kept across relaunches: the token names the
    /// room, not one launch.
    claude_inboxes: HashMap<usize, Arc<ClaudeInbox>>,
    /// Native rooms with no verified Codex thread, and the reason.
    unbound: HashMap<usize, (String, String)>,
    /// Rooms that reported themselves between sessions, keyed by destination.
    /// Their new mail is held instead of sent until a session is reported.
    pending_rooms: HashSet<String>,
    /// Whether each bound Codex room's process is still running. Shared with
    /// the adapter so a send is refused after the process exits.
    codex_live: HashMap<usize, Arc<AtomicBool>>,
    titles: HashMap<String, String>,
    reconciling: HashMap<String, Receiver<Delivery>>,
    last_reconcile: HashMap<String, Instant>,
    /// Commands the router is waiting an answer for, each with the id it
    /// was sent under. An event is matched by that id, never by position, so
    /// it can only ever reach the command that asked for it.
    waiting: Vec<(u64, Waiting)>,
    /// Id handed to the next command sent to the outbox worker. Ids start at
    /// 1; 0 is reserved for a failure no command is behind.
    command_ids: u64,
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

/// How severe a notice from the router is. The router sets this where
/// the notice is produced; the UI never guesses it from the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoticeSeverity {
    Info,
    Failure,
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
        let adapter = Arc::new(SwitchAdapter::new(rooms.clone())?);
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
        let mut codex = HashSet::new();
        let mut codex_live = HashMap::new();
        let mut codex_rooms = HashMap::new();
        let mut claude = HashSet::new();
        let mut claude_rooms = HashMap::new();
        let mut claude_inboxes = HashMap::new();
        for room in &rooms {
            bindings.insert(room.binding.room.clone(), room.binding.clone());
            titles.insert(room.binding.room.clone(), room.title.clone());
            match &room.backend {
                Backend::OpenCode(server) => {
                    servers.insert(room.binding.room.clone(), Arc::clone(server));
                }
                Backend::Codex(native) => {
                    codex.insert(room.binding.room.clone());
                    codex_live.insert(room.index, Arc::clone(&native.live));
                    codex_rooms.insert(room.index, room.clone());
                }
                Backend::Claude(inbox) => {
                    claude.insert(room.binding.room.clone());
                    claude_inboxes.insert(room.index, Arc::clone(inbox));
                    claude_rooms.insert(room.index, room.clone());
                }
            }
            by_index.insert(room.index, (room.title.clone(), room.binding.clone()));
        }
        let mut router = Self {
            outbox: Some(outbox),
            adapter: Reaped::new(adapter),
            path,
            rooms: by_index,
                                    bindings,
            servers: Reaped::new(servers),
            codex,
            codex_rooms,
            claude,
            claude_rooms,
            claude_inboxes,
            unbound: HashMap::new(),
            pending_rooms: HashSet::new(),
            codex_live,
            titles,
            reconciling: HashMap::new(),
            last_reconcile: HashMap::new(),
            waiting: Vec::new(),
            command_ids: 0,
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
            Ok(TaggedEvent {
                id,
                event: Event::Updated(messages),
            }) => {
                let _ = router.take_waiting(id);
                router.schedule_in_flight = false;
                router.messages = messages;
                Ok(router)
            }
            Ok(TaggedEvent {
                event: Event::Error(error),
                ..
            }) => Err(io::Error::other(format!(
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
        self.rooms.contains_key(&index) || self.unbound.contains_key(&index)
    }

    /// Binds the Codex rooms once each room's own thread is captured. A room
    /// without one stays native and unbound: mail to it is refused with a
    /// reason instead of being pasted or sent to another thread.
    pub(crate) fn bind_codex(&mut self, entries: Vec<CodexEntry>) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        for entry in entries {
            match entry.room {
                Some(room) => {
                    let destination = (entry.index + 1).to_string();
                    let binding =
                        crate::codex_native::binding(&destination, &room.thread, entry.generation);
                    self.unbound.remove(&entry.index);
                    self.codex.insert(destination.clone());
                    self.codex_live
                        .insert(entry.index, Arc::clone(&room.live));
                    self.titles
                        .insert(destination.clone(), entry.title.clone());
                    self.bindings
                        .insert(destination.clone(), binding.clone());
                    self.rooms
                        .insert(entry.index, (entry.title.clone(), binding.clone()));
                    self.codex_rooms.insert(
                        entry.index,
                        RoomEntry {
                            index: entry.index,
                            title: entry.title,
                            binding,
                            backend: Backend::Codex(room),
                        },
                    );
                }
                None => {
                    // A room that cannot start must drop the launch it replaces:
                    // an old binding would otherwise go on taking mail.
                    if let Some((_, stale)) = self.rooms.remove(&entry.index) {
                        self.bindings.remove(&stale.room);
                        self.codex.remove(&stale.room);
                        self.pending_rooms.remove(&stale.room);
                    }
                    self.codex_rooms.remove(&entry.index);
                    self.codex_live.remove(&entry.index);
                    let reason = entry.reason.unwrap_or_else(|| UNBOUND_CODEX.to_owned());
                    self.unbound.insert(entry.index, (entry.title, reason));
                }
            }
        }
        self.adapter.get().rebind(self.entries())
    }

    /// Binds the Claude rooms once each launch has its session id and inbox.
    /// A room without an inbox stays unbound: its mail is held, never pasted.
    pub(crate) fn bind_claude(&mut self, entries: Vec<ClaudeEntry>) -> io::Result<()> {
        for entry in entries {
            let destination = (entry.index + 1).to_string();
            match entry.session {
                Some(session) => {
                    let Some(inbox) = self.claude_inboxes.get(&entry.index).cloned() else {
                        self.unbound.insert(
                            entry.index,
                            (
                                entry.title,
                                "this room has no Claude inbox; restart Crowded".to_owned(),
                            ),
                        );
                        continue;
                    };
                    let binding =
                        crate::codex_native::binding(&destination, &session, entry.generation);
                    self.unbound.remove(&entry.index);
                    self.claude.insert(destination.clone());
                    self.titles
                        .insert(destination.clone(), entry.title.clone());
                    self.bindings
                        .insert(destination.clone(), binding.clone());
                    self.rooms
                        .insert(entry.index, (entry.title.clone(), binding.clone()));
                    self.claude_rooms.insert(
                        entry.index,
                        RoomEntry {
                            index: entry.index,
                            title: entry.title,
                            binding,
                            backend: Backend::Claude(inbox),
                        },
                    );
                }
                None => {
                    if let Some((_, stale)) = self.rooms.remove(&entry.index) {
                        self.bindings.remove(&stale.room);
                        self.claude.remove(&stale.room);
                        self.pending_rooms.remove(&stale.room);
                    }
                    self.claude_rooms.remove(&entry.index);
                    let reason = entry.reason.unwrap_or_else(|| UNBOUND_CLAUDE.to_owned());
                    self.unbound.insert(entry.index, (entry.title, reason));
                }
            }
        }
        self.adapter.get().rebind(self.entries())
    }

    /// Records whether a bound Codex room's process is still running. The
    /// adapter shares this flag, so queued mail for a room that exited is held
    /// instead of being queued into its old thread.
    pub(crate) fn set_codex_live(&mut self, index: usize, live: bool) {
        if let Some(flag) = self.codex_live.get(&index) {
            flag.store(live, Ordering::SeqCst);
        }
    }

    /// The room's current binding, if it has one.
    pub(crate) fn binding_for(&self, index: usize) -> Option<Binding> {
        self.rooms.get(&index).map(|(_, binding)| binding.clone())
    }

    /// The owned OpenCode server of a bound OpenCode room, if it has one.
    pub(crate) fn server_for(&self, index: usize) -> Option<Arc<Server>> {
        let binding = self.binding_for(index)?;
        self.servers.get().get(&binding.room).cloned()
    }

    /// The room's Claude inbox, bound on first use and then kept: its token
    /// names the room, so a relaunch reuses it.
    pub(crate) fn claude_inbox(&mut self, index: usize) -> io::Result<Arc<ClaudeInbox>> {
        if let Some(inbox) = self.claude_inboxes.get(&index) {
            return Ok(Arc::clone(inbox));
        }
        let inbox = Arc::new(ClaudeInbox::start()?);
        self.claude_inboxes.insert(index, Arc::clone(&inbox));
        Ok(inbox)
    }

    /// Rebinds a Claude room to a session its own hook reported. The mod's
    /// process did not restart, so its receiver stays bound and the inbox only
    /// moves to the reported generation.
    pub(crate) fn rebind_claude(
        &mut self,
        index: usize,
        title: String,
        session: &str,
        generation: u64,
    ) -> io::Result<()> {
        let inbox = self.claude_inbox(index)?;
        inbox.retarget(generation);
        self.rebind_room(RoomEntry {
            index,
            title,
            binding: crate::codex_native::binding(&(index + 1).to_string(), session, generation),
            backend: Backend::Claude(inbox),
        })
    }

    /// Whether the room reported itself between sessions. Its new mail is
    /// held until a session is reported.
    pub(crate) fn is_pending(&self, index: usize) -> bool {
        self.rooms
            .get(&index)
            .is_some_and(|(_, binding)| self.pending_rooms.contains(&binding.room))
    }

    /// Marks a bound room between-sessions. New mail is still saved, but the
    /// outbox holds it with a visible reason instead of sending it. The mark
    /// reaches the outbox before any mail saved after it, so the hold always
    /// applies to the right mail; a full outbox refuses loudly instead of
    /// letting mail through unheld.
    pub(crate) fn mark_pending(&mut self, index: usize) -> io::Result<()> {
        let Some(room) = self
            .rooms
            .get(&index)
            .map(|(_, binding)| binding.room.clone())
        else {
            return Err(io::Error::other("that room has no native binding"));
        };
        let mut rooms: Vec<String> = self.pending_rooms.iter().cloned().collect();
        rooms.push(room.clone());
        self.send_pending(&rooms)?;
        self.pending_rooms.insert(room);
        Ok(())
    }

    /// Clears a room's between-sessions mark and releases its pending-held
    /// mail onto its current binding. Sending first means a full outbox fails
    /// before anything changes.
    pub(crate) fn clear_pending(&mut self, index: usize) -> io::Result<()> {
        let Some((_, binding)) = self.rooms.get(&index).cloned() else {
            return Err(io::Error::other("that room has no native binding"));
        };
        self.send_retarget(&binding.room, &binding)?;
        let rooms: Vec<String> = self
            .pending_rooms
            .iter()
            .filter(|room| *room != &binding.room)
            .cloned()
            .collect();
        self.send_pending(&rooms)?;
        self.pending_rooms.remove(&binding.room);
        Ok(())
    }

    /// Tells the outbox which rooms are between sessions.
    fn send_pending(&mut self, rooms: &[String]) -> io::Result<()> {
        // The hold commands keep no waiter: they answer for themselves, so
        // their events can never take another command's reply.
        let id = self.next_command_id();
        self.outbox()
            .try_send(
                id,
                Command::SetPending {
                    rooms: rooms.to_vec(),
                },
            )
            .map_err(|_| io::Error::other("the native outbox is busy; try again"))?;
        Ok(())
    }

    /// Sends a room's pending-held mail onto a binding. Only [`PENDING_HOLD`]
    /// mail moves; anything held for another reason stays put.
    fn send_retarget(&mut self, room: &str, binding: &Binding) -> io::Result<()> {
        let id = self.next_command_id();
        self.outbox()
            .try_send(
                id,
                Command::Retarget {
                    room: room.to_owned(),
                    binding: binding.clone(),
                },
            )
            .map_err(|_| io::Error::other("the native outbox is busy; try again"))?;
        Ok(())
    }

    /// Drops a room's binding and holds it with a reason instead of pasting
    /// or sending its mail anywhere else. Saved mail for the old binding
    /// stays held as stale; new mail is refused with the reason.
    pub(crate) fn unbind_room(&mut self, index: usize, reason: String) -> io::Result<()> {
        let Some((title, binding)) = self.rooms.remove(&index) else {
            return Err(io::Error::other("that room has no native binding"));
        };
        self.bindings.remove(&binding.room);
        self.codex.remove(&binding.room);
        self.codex_rooms.remove(&index);
        self.codex_live.remove(&index);
        self.pending_rooms.remove(&binding.room);
        self.unbound.insert(index, (title, reason));
        self.adapter.get().rebind(self.entries())?;
        Ok(())
    }

    /// Rebinds one room to a new launch generation and session. Every other
    /// room keeps its binding and adapter: the switching adapter is rebuilt
    /// from the full room set, never from the rebound room alone. Pending-held
    /// mail for the room is adopted onto the new binding; mail held for any
    /// other reason stays put.
    pub(crate) fn rebind_room(&mut self, entry: RoomEntry) -> io::Result<()> {
        if entry.binding.room.is_empty() || entry.binding.generation == 0 {
            return Err(io::Error::other("a rebinding needs its room and generation"));
        }
        let index = entry.index;
        let destination = entry.binding.room.clone();
        if let Backend::OpenCode(_) = entry.backend
            && !self.servers.get().contains_key(&destination)
        {
            return Err(io::Error::other(
                "that room has no owned OpenCode server",
            ));
        }
        let rooms: Vec<String> = self
            .pending_rooms
            .iter()
            .filter(|room| *room != &destination)
            .cloned()
            .collect();
        self.send_pending(&rooms)?;
        self.send_retarget(&destination, &entry.binding)?;
        if let Backend::OpenCode(server) = &entry.backend {
            server.adopt(&entry.binding).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("that room could not move to the reported session: {error}"),
                )
            })?;
        }
        self.rooms
            .insert(index, (entry.title.clone(), entry.binding.clone()));
        self.bindings
            .insert(destination.clone(), entry.binding.clone());
        self.titles
            .insert(destination.clone(), entry.title.clone());
        match &entry.backend {
            Backend::OpenCode(_) => {
                self.codex.remove(&destination);
            }
            Backend::Codex(native) => {
                self.codex.insert(destination.clone());
                self.codex_live.insert(index, Arc::clone(&native.live));
                self.codex_rooms.insert(index, entry.clone());
            }
            Backend::Claude(_) => {
                self.claude.insert(destination.clone());
                self.claude_rooms.insert(index, entry.clone());
            }
        }
        self.unbound.remove(&index);
        self.pending_rooms.remove(&destination);
        self.adapter.get().rebind(self.entries())?;
        Ok(())
    }

    /// The currently bound rooms, as the switching adapter needs them.
    fn entries(&self) -> Vec<RoomEntry> {
        let mut entries: Vec<RoomEntry> = self
            .rooms
            .iter()
            .filter_map(|(index, (title, binding))| {
                let server = self.servers.get().get(&binding.room)?;
                Some(RoomEntry {
                    index: *index,
                    title: title.clone(),
                    binding: binding.clone(),
                    backend: Backend::OpenCode(Arc::clone(server)),
                })
            })
            .collect();
        entries.extend(self.codex_rooms.values().cloned());
        entries.extend(self.claude_rooms.values().cloned());
        entries
    }

    /// Replaces the bound rooms (a new launch generation). Mail for a binding
    /// that is no longer current stays in the outbox as held; it is never
    /// moved to the new binding.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rebind(&mut self, rooms: Vec<RoomEntry>) -> io::Result<()> {
        self.adapter.get().rebind(rooms.clone())?;
                        self.bindings.clear();
        let mut servers = HashMap::new();
        self.rooms.clear();
        self.codex.clear();
        self.codex_rooms.clear();
        self.claude.clear();
        self.claude_rooms.clear();
        self.unbound.clear();
        self.codex_live.clear();
        for room in &rooms {
            self.bindings
                .insert(room.binding.room.clone(), room.binding.clone());
            self.titles
                .insert(room.binding.room.clone(), room.title.clone());
            self.rooms
                .insert(room.index, (room.title.clone(), room.binding.clone()));
            match &room.backend {
                Backend::OpenCode(server) => {
                    servers.insert(room.binding.room.clone(), Arc::clone(server));
                }
                Backend::Codex(native) => {
                    self.codex.insert(room.binding.room.clone());
                    self.codex_live.insert(room.index, Arc::clone(&native.live));
                    self.codex_rooms.insert(room.index, room.clone());
                }
                Backend::Claude(inbox) => {
                    self.claude.insert(room.binding.room.clone());
                    self.claude_inboxes.insert(room.index, Arc::clone(inbox));
                    self.claude_rooms.insert(room.index, room.clone());
                }
            }
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
        // A room whose process exited cannot be sent mail: the CLI would queue
        // it into a thread this launch no longer owns.
        if self
            .codex_live
            .get(&to)
            .is_some_and(|live| !live.load(Ordering::SeqCst))
        {
            let title = self
                .rooms
                .get(&to)
                .map(|(title, _)| title.clone())
                .unwrap_or_else(|| format!("room {}", to + 1));
            let error = format!(
                "{title} is not running; native mail is held until it is relaunched"
            );
            if let Some(reply) = reply {
                reply(0, NativeReply::Failed(error.clone()));
            }
            return Err(error);
        }
        let Some((title, binding)) = self.rooms.get(&to).cloned() else {
            if let Some((title, reason)) = self.unbound.get(&to) {
                let error = format!("{title} cannot receive native mail: {reason}");
                if let Some(reply) = reply {
                    reply(0, NativeReply::Failed(error.clone()));
                }
                return Err(error);
            }
            let error = "that room does not use native delivery".to_owned();
            if let Some(reply) = reply {
                reply(0, NativeReply::Failed(error.clone()));
            }
            return Err(error);
        };
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
        let id = self.next_command_id();
        match self.outbox().try_send(id, command) {
            Ok(()) => {
                self.waiting.push((id, Waiting::Enqueue { envelope, reply }));
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
    /// Keeps the ordinary scheduling interval from elapsing, so a test can
    /// drain one command's events without a later snapshot arriving to change
    /// what the mailroom shows.
    #[cfg(test)]
    pub(crate) fn hold_scheduling(&mut self) {
        self.last_schedule = Some(Instant::now());
    }

    /// Advances the outbox and collects what it reports. Call it every pass of
    /// the event loop; it never blocks. Returns short notices worth showing.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        mailroom: &mut Mailroom,
        paused: bool,
        fused: bool,
    ) -> Vec<(String, NoticeSeverity)> {
        let mut notices = self.drain(now, mailroom);
        self.paused = paused || fused;
        if self.unavailable.is_none()
            && !self.schedule_in_flight
            && self
                .last_schedule
                .is_none_or(|last| now.duration_since(last) >= SCHEDULE_INTERVAL)
        {
            if let Err(error) = self.send_schedule(paused, fused) {
                notices.push((error.to_string(), NoticeSeverity::Failure));
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
            let command_id = self.next_command_id();
            if self.outbox().try_send(command_id, command).is_ok() {
                self.waiting.push((command_id, Waiting::Resolve));
            }
        }
        for message in &self.messages {
            if message.status != Status::Uncertain
                // Codex has no caller-selected idempotency key, so an uncertain
                // queue attempt stays for an operator instead of being looked up.
                || self.codex.contains(&message.destination)
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
            pending: self.pending_rooms.iter().cloned().collect(),
        };
        let id = self.next_command_id();
        match self.outbox().try_send(id, command) {
            Ok(()) => {
                self.waiting.push((id, Waiting::Schedule));
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

    /// The id the next command sent to the outbox will carry.
    fn next_command_id(&mut self) -> u64 {
        self.command_ids += 1;
        self.command_ids
    }

    /// The waiter registered for `id`, removed. Matching is by id, so an
    /// event can only ever reach the command that asked for it; an event
    /// whose id has no waiter changes nothing.
    fn take_waiting(&mut self, id: u64) -> Option<Waiting> {
        let position = self.waiting.iter().position(|(key, _)| *key == id)?;
        Some(self.waiting.remove(position).1)
    }

    fn drain(&mut self, now: Instant, mailroom: &mut Mailroom) -> Vec<(String, NoticeSeverity)> {
        let mut notices = Vec::new();
        while let Ok(TaggedEvent { id, event }) = self.outbox().events.try_recv() {
            match event {
                Event::Queued(queued) => {
                    if let Some(Waiting::Enqueue { envelope, reply }) = self.take_waiting(id) {
                        self.envelope_of.insert(queued.id.clone(), envelope);
                        // The message was saved with this status, so a hold
                        // shows from the moment it exists rather than at the
                        // next scheduling snapshot.
                        let text = match &queued.status {
                            Status::Queued { hold: Some(reason) } => held_text(reason),
                            _ => "QUEUED: saved, not yet sent".to_owned(),
                        };
                        mailroom.set_native_status(envelope, text);
                        if let Some(reply) = reply {
                            self.pending.push(PendingReply {
                                outbox_id: queued.id,
                                envelope,
                                reply,
                                deadline: now + REPLY_WAIT,
                            });
                        }
                    }
                }
                Event::Updated(messages) => {
                    // Only the command that asked for this answer acts on it:
                    // an update from a hold command, which keeps no waiter,
                    // leaves a waiting schedule exactly where it was.
                    match self.take_waiting(id) {
                        Some(Waiting::Schedule) => self.schedule_in_flight = false,
                        Some(Waiting::Resolve) | Some(Waiting::Enqueue { .. }) | None => {}
                    }
                    self.messages = messages;
                    self.sync_mailroom(mailroom);
                }
                // A hold command answers for itself, with the rooms it was
                // about: it never consumes a waiter belonging to another
                // command, and it is never swallowed.
                Event::HoldFailed { rooms, error } => {
                    let _ = self.take_waiting(id);
                    notices.push((
                        format!(
                            "native delivery: {} could not be held: {error}",
                            self.names(&rooms)
                        ),
                        NoticeSeverity::Failure,
                    ));
                }
                Event::Error(error) => match self.take_waiting(id) {
                    Some(Waiting::Enqueue { envelope, reply }) => {
                        mailroom.set_native_status(envelope, format!("FAILED: {error}"));
                        if let Some(reply) = reply {
                            reply(envelope, NativeReply::Failed(error));
                        }
                    }
                                        Some(Waiting::Schedule) => {
                        self.schedule_in_flight = false;
                        notices.push((format!("native delivery: {error}"), NoticeSeverity::Failure));
                    }
                    Some(Waiting::Resolve) => {
                        notices.push((format!("native delivery: {error}"), NoticeSeverity::Failure));
                    }
                    None => {
                        // No command is waiting for this id: the failure
                        // belongs to the outbox itself, not to a sender.
                        self.unavailable = Some(error.clone());
                        notices.push((
                            format!("native delivery stopped: {error}"),
                            NoticeSeverity::Failure,
                        ));
                    }
                },
            }
        }
        notices
    }

    /// How a notice names a set of rooms: their titles where known.
    fn names(&self, rooms: &[String]) -> String {
        rooms
            .iter()
            .map(|room| {
                self.titles
                    .get(room)
                    .map(String::as_str)
                    .unwrap_or(room.as_str())
            })
            .collect::<Vec<_>>()
            .join(", ")
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
            Status::Queued { hold: Some(reason) } => held_text(reason),
            Status::Sending => "SENDING".to_owned(),
            Status::Accepted => {
                if self.codex.contains(&message.destination) {
                    "ACCEPTED by Codex (queued there; not proof it was read)".to_owned()
                } else if self.claude.contains(&message.destination) {
                    "ACCEPTED by Claude (submitted; not proof it was acted on)".to_owned()
                } else {
                    "ACCEPTED by OpenCode (queued there; not proof it was read)".to_owned()
                }
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

/// The mailroom line for a held message. Both the moment the message is
/// saved and every later snapshot go through it, so a hold never changes its
/// wording as the snapshots arrive.
fn held_text(reason: &str) -> String {
    format!("HELD: {reason}")
}

#[cfg(test)]
impl NativeRouter {
    pub(crate) fn poison_switching_adapter(&self) {
        let adapter = Arc::clone(self.adapter.get());
        let _ = std::thread::spawn(move || {
            let _guard = adapter.current.lock().unwrap();
            panic!("poison switching adapter");
        })
        .join();
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
            backend: Backend::OpenCode(launched.room.server()),
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

    /// Ticks until both messages have an answer of their own, collecting the
    /// notices each pass drained.
    fn answered(
        router: &mut NativeRouter,
        mailroom: &mut Mailroom,
        notices: &mut Vec<String>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            notices.extend(
                router
                    .tick(Instant::now(), mailroom, false, false)
                    .into_iter()
                    .map(|(text, _)| text),
            );
            if !mailroom.summary().contains("SAVING") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never answered:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A hold that succeeds, a schedule that fails once and new mail sent
    /// before any drain: each answer reaches the command that asked for it,
    /// so the new message is never blamed for the schedule failure.
    #[test]
    fn a_hold_success_a_schedule_failure_and_an_enqueue_stay_separate() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let path = state_path(root.path());
        let mut router =
            NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();

        router
            .send(&mut mailroom, "Room X", 0, "old", recorder(&log))
            .unwrap();
        // The schedule issued right after the pending report fails once.
        crate::native_outbox::fail_next_schedule_at(&path);
        router.mark_pending(0).unwrap();
        router.send_schedule(false, false).unwrap();
        router
            .send(&mut mailroom, "Room X", 0, "new", recorder(&log))
            .unwrap();

        let mut notices = Vec::new();
        answered(&mut router, &mut mailroom, &mut notices);
        let view = mailroom.summary();
        assert!(
            !view.contains("FAILED"),
            "the schedule failure is not the message's: {view}"
        );
        assert!(
            view.contains("session pending after clear"),
            "the new message keeps its own held result, not the schedule's: {view}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("injected schedule failure")),
            "the failure is reported against the schedule: {notices:?}"
        );

        // Nothing is stuck: releasing the hold delivers the new message.
        router.clear_pending(0).unwrap();
        pump(
            &mut router,
            &mut mailroom,
            false,
            false,
            |mailroom| mailroom.summary().contains("ACCEPTED by OpenCode"),
        );
        assert!(
            texts(&a).iter().any(|text| text.contains("new")),
            "{:?}",
            texts(&a)
        );
    }

    /// An event whose id no waiter holds changes nothing: it never reaches a
    /// command that is waiting for a different answer.
    #[test]
    fn an_event_with_an_unknown_id_reaches_no_waiter() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();

        // A command nothing is waiting for, answered before the real ones.
        router
            .outbox()
            .try_send(9_999, Command::SetPending { rooms: Vec::new() })
            .unwrap();
        router.send_schedule(false, false).unwrap();
        router
            .send(&mut mailroom, "Room X", 0, "hello", recorder(&log))
            .unwrap();
        assert_eq!(router.waiting.len(), 2, "two commands wait for answers");

        // The enqueue's own answer is last in the queue, so once it lands
        // every earlier event has been drained too.
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.tick(Instant::now(), &mut mailroom, false, false);
            if !mailroom.summary().contains("SAVING") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never answered:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !router
                .waiting
                .iter()
                .any(|(_, waiting)| matches!(waiting, Waiting::Enqueue { .. })),
            "the enqueue was answered by its own event, not the unknown one"
        );
        let view = mailroom.summary();
        assert!(!view.contains("SAVING"), "{view}");
        pump(
            &mut router,
            &mut mailroom,
            false,
            false,
            |_| !log.borrow().is_empty(),
        );
        assert!(
            !matches!(log.borrow()[0].1, NativeReply::Failed(_)),
            "the message keeps its own result: {:?}",
            log.borrow()[0].1
        );
    }

    /// Paused and fused scheduling around a failing hold and a failing
    /// resolve: every message keeps the answer it actually earned.
    #[test]
    fn failed_hold_and_resolve_commands_leave_every_message_with_its_own_result() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let path = state_path(root.path());
        // The server reads the first message and never replies to it.
        *a.fake.state.mode.lock().unwrap() = FakeMode::Drop;
        let mut router = NativeRouter::start(path.clone(), vec![entry(&a, 0, "Room A")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router
            .send(&mut mailroom, "Room X", 0, "lost reply", recorder(&log))
            .unwrap();
        pump(
            &mut router,
            &mut mailroom,
            false,
            false,
            |_| !log.borrow().is_empty(),
        );
        assert_eq!(log.borrow()[0].1, NativeReply::Uncertain);

        // A second message is saved and then held for the next session.
        *a.fake.state.mode.lock().unwrap() = FakeMode::Normal;
        router.mark_pending(0).unwrap();
        router
            .send(&mut mailroom, "Room X", 0, "held", None)
            .unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 200);

        // The disk goes away: adopting the held mail fails, resolving the
        // uncertain message fails, and a message saved from now on cannot be
        // persisted either.
        std::fs::remove_dir_all(root.path().join("state")).unwrap();
        std::fs::write(root.path().join("state"), b"not a directory").unwrap();
        router.clear_pending(0).unwrap();
        let uncertain = router
            .messages
            .iter()
            .find(|message| message.status == Status::Uncertain)
            .cloned()
            .expect("the uncertain message is still there");
        let resolve_id = router.next_command_id();
        router.waiting.push((resolve_id, Waiting::Resolve));
        router
            .outbox()
            .try_send(
                resolve_id,
                Command::Resolve {
                    id: uncertain.id.clone(),
                    binding: uncertain.binding.clone(),
                    hash: uncertain.hash.clone(),
                    outcome: Resolution::Accepted(None),
                },
            )
            .unwrap();
        router
            .send(&mut mailroom, "Room X", 0, "doomed", None)
            .unwrap();
        // Fused scheduling drains the same answers as paused scheduling does.
        pump_for(&mut router, &mut mailroom, false, true, 300);

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.tick(Instant::now(), &mut mailroom, false, false);
            if !mailroom.summary().contains("SAVING") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never answered:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
        let view = mailroom.summary();
        assert!(view.contains("UNCERTAIN"), "{view}");
        assert!(view.contains("HELD"), "{view}");
        assert!(
            view.contains("FAILED"),
            "the doomed message reports its own failed save: {view}"
        );
        assert!(
            router.unavailable.is_none(),
            "both failures were attributed to their commands, not left fatal"
        );
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
        let adapter = SwitchAdapter::new(vec![entry(&a, 0, "Room A")]).unwrap();
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

    // --- Codex native delivery ---

    fn codex_stub(root: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn codex_room(program: &Path, cwd: &Path, thread: &str) -> Arc<crate::codex_native::Room> {
        Arc::new(crate::codex_native::room(
            program.as_os_str().to_owned(),
            cwd,
            thread.to_owned(),
            Vec::new(),
        ))
    }

    fn codex_entry(index: usize, title: &str, room: Option<Arc<crate::codex_native::Room>>) -> CodexEntry {
        codex_entry_at(index, title, room, 1)
    }

    fn codex_entry_at(
        index: usize,
        title: &str,
        room: Option<Arc<crate::codex_native::Room>>,
        generation: u64,
    ) -> CodexEntry {
        CodexEntry {
            index,
            title: title.to_owned(),
            room,
            generation,
            reason: None,
        }
    }

    fn args_log(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn codex_mail_is_queued_with_the_exact_text_and_reported_as_codex_acceptance() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log_path = root.path().join("args");
        let program = codex_stub(
            root.path(),
            "codex",
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' 'thread-a'",
                log_path.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![codex_entry(0, "Room A", Some(codex_room(&program, cwd.path(), "thread-a")))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        router
            .send(&mut mailroom, "Room B", 0, "multi\nline \u{2713} \u{fc}", None)
            .unwrap();
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("ACCEPTED by Codex")
        });

        // The text arrives as one exact argument, envelope note and all.
        let arguments = args_log(&log_path);
        assert_eq!(&arguments[..4], ["queue", "--thread", "thread-a", "--message"]);
        assert_eq!(
            arguments[4..].join("\n"),
            "[whisper from Room B] multi\nline \u{2713} \u{fc}"
        );

        let view = mailroom.summary();
        assert!(
            view.contains("ACCEPTED by Codex (queued there; not proof it was read)"),
            "{view}"
        );
    }

    fn marking_codex_room(root: &Path, cwd: &Path, name: &str) -> (Arc<crate::codex_native::Room>, PathBuf) {
        let marker = root.join(format!("{name}.args"));
        let program = codex_stub(
            root,
            name,
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' '{name}'",
                marker.display()
            ),
        );
        (codex_room(&program, cwd, name), marker)
    }

    fn deliver_to(router: &mut NativeRouter, mailroom: &mut Mailroom, index: usize, marker: &Path) {
        router.send(mailroom, "Room X", index, "hi", None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !marker.exists() {
            router.tick(Instant::now(), mailroom, false, false);
            assert!(Instant::now() < deadline, "never delivered:\n{}", mailroom.summary());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn binding_codex_rooms_one_after_another_keeps_every_earlier_room_deliverable() {
        for order in [[0usize, 1], [1, 0]] {
            let root = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let rooms = [
                marking_codex_room(root.path(), cwd.path(), "thread-a"),
                marking_codex_room(root.path(), cwd.path(), "thread-b"),
            ];
            let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
            for index in order {
                let entry = codex_entry(index, &format!("Room {index}"), Some(Arc::clone(&rooms[index].0)));
                router.bind_codex(vec![entry]).unwrap();
            }
            // A third room that is still pending or failed must not disturb them.
            router.bind_codex(vec![codex_entry(2, "Room 2", None)]).unwrap();
            let mut mailroom = Mailroom::new(50);
            for index in 0..2 {
                deliver_to(&mut router, &mut mailroom, index, &rooms[index].1);
            }
        }
    }

    #[test]
    fn codex_rooms_bound_later_do_not_drop_the_opencode_rooms() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let open = launch(root.path(), cwd.path(), "a", 1, 1);
        let (codex, marker) = marking_codex_room(root.path(), cwd.path(), "thread-b");
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&open, 0, "Room A")]).unwrap();
        router.bind_codex(vec![codex_entry(1, "Room B", Some(codex))]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room X", 0, "to opencode", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(texts(&open).len(), 1);
        deliver_to(&mut router, &mut mailroom, 1, &marker);
    }

    /// A RoomEntry that moves an existing fake-server room to a session the
    /// fake server itself echoes (ses_<tag>), on the same owned server. The
    /// fake answers every prompt with its own tag, so only that id can be
    /// delivered in tests; the point under test is the rebinding, not the id.
    fn rebound_opencode(
        launched: &Launched,
        index: usize,
        title: &str,
        session: &str,
        generation: u64,
    ) -> RoomEntry {
        RoomEntry {
            index,
            title: title.to_owned(),
            binding: crate::native_outbox::Binding {
                room: launched.room.binding().room.clone(),
                session: session.to_owned(),
                endpoint: launched.room.binding().endpoint.clone(),
                generation,
            },
            backend: Backend::OpenCode(launched.room.server()),
        }
    }

    /// Like [`entry`], but the room starts bound to a session that was never
    /// submitted anywhere, so mail saved before any rebind stays queued and
    /// unsent instead of conflicting with the fake server's echo.
    fn entry_with_session(
        launched: &Launched,
        index: usize,
        title: &str,
        session: &str,
    ) -> RoomEntry {
        let mut rebound = entry(launched, index, title);
        rebound.binding.session = session.to_owned();
        rebound
    }

    #[test]
    fn rebinding_one_room_keeps_every_other_room_on_its_own_session() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let (codex, marker) = marking_codex_room(root.path(), cwd.path(), "thread-a");
        let mut router = NativeRouter::start(
            state_path(root.path()),
            vec![
                entry(&a, 0, "Room A"),
                entry_with_session(&b, 1, "Room B", "ses-old-b"),
            ],
        )
        .unwrap();
        router
            .bind_codex(vec![codex_entry(2, "Room C", Some(codex))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        // Old mail for B is saved before the rebind and never sent.
        router.send(&mut mailroom, "Room X", 1, "old mail for B", None).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 300);
        router
            .rebind_room(rebound_opencode(&b, 1, "Room B", "ses_b", 2))
            .unwrap();
        let log = Log::default();
        for (index, text) in [(0, "to A"), (1, "to B new"), (2, "to C")] {
            router
                .send(&mut mailroom, "Room X", index, text, recorder(&log))
                .unwrap();
        }
        pump(&mut router, &mut mailroom, false, false, |_| log.borrow().len() == 3);
        pump_for(&mut router, &mut mailroom, false, false, 300);
        assert_eq!(texts(&a), ["[whisper from Room X] to A"]);
        assert_eq!(texts(&b), ["[whisper from Room X] to B new"]);
        let arguments = args_log(&marker);
        assert_eq!(&arguments[..4], ["queue", "--thread", "thread-a", "--message"]);
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
        let binding = router.binding_for(1).expect("B stays bound");
        assert_eq!(binding.session, "ses_b");
        assert_eq!(binding.generation, 2);
    }

    #[test]
    fn pending_mail_is_held_then_delivered_to_the_reported_session() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let mut router = NativeRouter::start(
            state_path(root.path()),
            vec![entry_with_session(&b, 1, "Room B", "ses-old-b")],
        )
        .unwrap();
        router.mark_pending(1).unwrap();
        assert!(router.is_pending(1));
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router
            .send(&mut mailroom, "Room X", 1, "held mail", recorder(&log))
            .unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(
            log.borrow()[0].1,
            NativeReply::Held("session pending after clear".to_owned())
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: session pending after clear"), "{view}");
        assert!(texts(&b).is_empty(), "nothing is sent while pending");
        router
            .rebind_room(rebound_opencode(&b, 1, "Room B", "ses_b", 2))
            .unwrap();
        assert!(!router.is_pending(1));
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("ACCEPTED by OpenCode")
        });
        assert_eq!(texts(&b), ["[whisper from Room X] held mail"]);
        let binding = router.binding_for(1).expect("B stays bound");
        assert_eq!(binding.session, "ses_b");
        assert_eq!(binding.generation, 2);
    }

    /// A message saved while its room is between sessions is held the moment
    /// it is saved: the room view must say so before any scheduling snapshot
    /// exists to repeat it, because that snapshot can be a whole interval
    /// away or can fail. Scheduling is held for the whole probe.
    #[test]
    fn a_message_saved_while_pending_is_shown_held_before_any_snapshot() {
        for (paused, fused) in [(false, false), (true, false), (false, true)] {
            let root = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let b = launch(root.path(), cwd.path(), "b", 2, 1);
            let mut router =
                NativeRouter::start(state_path(root.path()), vec![entry(&b, 1, "Room B")]).unwrap();
            let mut mailroom = Mailroom::new(50);
            let log = Log::default();

            router.mark_pending(1).unwrap();
            router
                .send(&mut mailroom, "Room X", 1, "held mail", recorder(&log))
                .unwrap();

            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                router.hold_scheduling();
                router.tick(Instant::now(), &mut mailroom, paused, fused);
                if !mailroom.summary().contains("SAVING") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "never saved (paused {paused}, fused {fused}):\n{}",
                    mailroom.summary()
                );
                thread::sleep(Duration::from_millis(20));
            }
            let view = mailroom.summary();
            assert!(
                view.contains("HELD: session pending after clear"),
                "the hold shows from the save alone (paused {paused}, fused {fused}): {view}"
            );
            assert!(
                !view.contains("QUEUED: saved, not yet sent"),
                "held mail must not read as plain QUEUED (paused {paused}, fused {fused}): {view}"
            );
        }
    }

    /// The ordinary case is unchanged: mail saved while the room is running
    /// still reads as queued until something sends it.
    #[test]
    fn a_message_saved_while_running_is_shown_queued_until_it_is_sent() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&b, 1, "Room B")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();

        router
            .send(&mut mailroom, "Room X", 1, "queued mail", recorder(&log))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.hold_scheduling();
            router.tick(Instant::now(), &mut mailroom, false, false);
            if !mailroom.summary().contains("SAVING") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never saved:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
        let view = mailroom.summary();
        assert!(view.contains("QUEUED: saved, not yet sent"), "{view}");
        assert!(!view.contains("HELD:"), "running mail is not held: {view}");
    }

    /// The same-session report delivers the mail that was held for it, while
    /// mail saved before the mark belongs to the session that ended and stays
    /// held through the very same clear.
    #[test]
    fn a_same_session_report_delivers_the_held_mail_and_leaves_the_older_one_held() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&b, 1, "Room B")]).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();

        router
            .send(&mut mailroom, "Room X", 1, "older mail", recorder(&log))
            .unwrap();
        router.mark_pending(1).unwrap();
        router
            .send(&mut mailroom, "Room X", 1, "held mail", recorder(&log))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.hold_scheduling();
            router.tick(Instant::now(), &mut mailroom, false, false);
            let view = mailroom.summary();
            if view.contains("HELD: session pending after clear")
                && view.contains("HELD: sent for an earlier launch")
            {
                break;
            }
            assert!(Instant::now() < deadline, "never held:\n{view}");
            thread::sleep(Duration::from_millis(20));
        }

        router.clear_pending(1).unwrap();
        assert!(!router.is_pending(1));
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("ACCEPTED by OpenCode")
        });
        let view = mailroom.summary();
        assert!(!view.contains("HELD: session pending after clear"), "{view}");
        assert!(
            view.contains("HELD: sent for an earlier launch"),
            "the mail saved before the mark stays held: {view}"
        );
        assert_eq!(texts(&b), ["[whisper from Room X] held mail"]);
    }

    #[test]
    fn the_same_session_id_while_pending_releases_mail_without_a_rebind() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let b = launch(root.path(), cwd.path(), "b", 2, 1);
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&b, 1, "Room B")]).unwrap();
        router.mark_pending(1).unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router
            .send(&mut mailroom, "Room X", 1, "held mail", recorder(&log))
            .unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(
            log.borrow()[0].1,
            NativeReply::Held("session pending after clear".to_owned())
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: session pending"), "{view}");
        router.clear_pending(1).unwrap();
        assert!(!router.is_pending(1));
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("ACCEPTED by OpenCode")
        });
        assert_eq!(texts(&b), ["[whisper from Room X] held mail"]);
        let binding = router.binding_for(1).expect("B stays bound");
        assert_eq!(binding.generation, 1, "no generation bump for the same id");
    }

    #[test]
    fn rebinding_a_codex_room_moves_its_thread_and_keeps_the_others() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = launch(root.path(), cwd.path(), "a", 1, 1);
        let (codex_a, marker_a) = marking_codex_room(root.path(), cwd.path(), "thread-a");
        let mut router =
            NativeRouter::start(state_path(root.path()), vec![entry(&a, 0, "Room A")]).unwrap();
        router
            .bind_codex(vec![codex_entry(1, "Room B", Some(codex_a))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        router.send(&mut mailroom, "Room X", 1, "before", None).unwrap();
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().matches("ACCEPTED by Codex").count() == 1
        });
        let (codex_b, marker_b) = marking_codex_room(root.path(), cwd.path(), "thread-b");
        router
            .rebind_room(RoomEntry {
                index: 1,
                title: "Room B".to_owned(),
                binding: crate::codex_native::binding("2", "thread-b", 2),
                backend: Backend::Codex(codex_b),
            })
            .unwrap();
        router.send(&mut mailroom, "Room X", 1, "after", None).unwrap();
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().matches("ACCEPTED by Codex").count() == 2
        });
        let arguments = args_log(&marker_b);
        assert_eq!(&arguments[..4], ["queue", "--thread", "thread-b", "--message"]);
        assert_eq!(&arguments[4..].join("\n"), "[whisper from Room X] after");
        assert_eq!(args_log(&marker_a).len(), 5, "the old thread got exactly one message");
        // The OpenCode room is untouched by the Codex rebind.
        let log = Log::default();
        router.send(&mut mailroom, "Room X", 0, "still A", recorder(&log)).unwrap();
        pump(&mut router, &mut mailroom, false, false, |_| !log.borrow().is_empty());
        assert_eq!(texts(&a), ["[whisper from Room X] still A"]);
    }

    #[test]
    fn an_unbound_codex_room_is_still_native_and_holds_mail_with_a_reason() {
        let root = tempfile::tempdir().unwrap();
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router.bind_codex(vec![codex_entry(0, "Room A", None)]).unwrap();
        assert!(router.is_native(0));
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        let error = router
            .send(&mut mailroom, "Room B", 0, "hello", recorder(&log))
            .unwrap_err();
        assert!(error.contains("Room A") && error.contains("no Codex session"), "{error}");
        assert_eq!(log.borrow()[0].1, NativeReply::Failed(error));
        assert_eq!(mailroom.summary().matches("Room B").count(), 0);
    }

    #[test]
    fn a_codex_attempt_that_times_out_after_submitting_is_uncertain_and_never_resent() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let count = root.path().join("count");
        let program = codex_stub(
            root.path(),
            "codex",
            &format!(
                "printf x >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' 'thread-a'\nexec sleep 30",
                count.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![codex_entry(0, "Room A", Some(codex_room(&program, cwd.path(), "thread-a")))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Room B", 0, "once", recorder(&log)).unwrap();
        // The attempt outlives the caller's wait, so the answer is "saved,
        // still waiting"; the message later becomes uncertain.
        pump(&mut router, &mut mailroom, false, false, |view| {
            view.summary().contains("UNCERTAIN: may have been accepted")
        });
        assert!(matches!(
            log.borrow()[0].1,
            NativeReply::Queued | NativeReply::Uncertain
        ));
        // Several more schedule passes must not queue it a second time.
        pump_for(&mut router, &mut mailroom, false, false, 1200);
        assert_eq!(fs::read_to_string(&count).unwrap(), "x");
        let view = mailroom.summary();
        assert!(
            view.contains("UNCERTAIN: may have been accepted; it will not be resent"),
            "{view}"
        );
    }

    #[test]
    fn a_rejected_codex_message_fails_visibly_and_does_not_block_another_room() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let failing = root.path().join("failing-args");
        let healthy = root.path().join("healthy-args");
        let a = codex_stub(
            root.path(),
            "codex-a",
            &format!(
                "printf '%s\\n' \"$5\" >> '{}'\nprintf 'Error: thread/queue/add failed: no rollout found\\n' >&2\nexit 1",
                failing.display()
            ),
        );
        let b = codex_stub(
            root.path(),
            "codex-b",
            &format!(
                "printf '%s\\n' \"$5\" >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' 'thread-b'",
                healthy.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![
                codex_entry(0, "Room A", Some(codex_room(&a, cwd.path(), "thread-a"))),
                codex_entry(1, "Room B", Some(codex_room(&b, cwd.path(), "thread-b"))),
            ])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        for (to, text) in [(0, "one"), (0, "two"), (1, "three")] {
            router.send(&mut mailroom, "Sender", to, text, None).unwrap();
        }
        pump(&mut router, &mut mailroom, false, false, |view| {
            let view = view.summary();
            args_log(&failing).len() == 2
                && args_log(&healthy).len() == 1
                && view.contains("FAILED: thread/queue/add failed")
                && view.contains("ACCEPTED by Codex")
        });
        // Order is preserved inside the failing room, and the healthy room's
        // mail is unaffected by it.
        assert_eq!(
            args_log(&failing),
            ["[whisper from Sender] one", "[whisper from Sender] two"]
        );
        assert_eq!(args_log(&healthy), ["[whisper from Sender] three"]);
        let view = mailroom.summary();
        assert!(view.contains("FAILED: thread/queue/add failed"), "{view}");
        assert_eq!(view.matches("ACCEPTED by Codex").count(), 1, "{view}");
    }

    #[test]
    fn codex_mail_for_an_earlier_launch_is_held_and_never_queued() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let count = root.path().join("count");
        let program = codex_stub(
            root.path(),
            "codex",
            &format!(
                "printf x >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' \"$3\"",
                count.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![codex_entry(0, "Room A", Some(codex_room(&program, cwd.path(), "thread-a")))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        let log = Log::default();
        router.send(&mut mailroom, "Sender", 0, "stale", recorder(&log)).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 400);

        // A new launch of the same room: new thread, and the saved mail belongs
        // to the old one.
        router
            .bind_codex(vec![codex_entry(0, "Room A", Some(codex_room(&program, cwd.path(), "thread-b")))])
            .unwrap();
        pump_for(&mut router, &mut mailroom, false, false, 600);
        assert!(!count.exists(), "mail for an earlier launch was queued");
        assert!(mailroom.summary().contains("HELD: sent for an earlier launch"));
    }

    #[test]
    fn a_new_generation_of_the_same_thread_rejects_mail_saved_for_the_old_one() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let count = root.path().join("count");
        let program = codex_stub(
            root.path(),
            "codex",
            &format!(
                "printf x >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' \"$3\"",
                count.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![codex_entry_at(
                0,
                "Room A",
                Some(codex_room(&program, cwd.path(), "thread-a")),
                1,
            )])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        router.send(&mut mailroom, "Sender", 0, "old launch", None).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 400);

        // Same session id, new launch generation.
        router
            .bind_codex(vec![codex_entry_at(
                0,
                "Room A",
                Some(codex_room(&program, cwd.path(), "thread-a")),
                2,
            )])
            .unwrap();
        pump_for(&mut router, &mut mailroom, false, false, 600);
        assert!(!count.exists(), "mail from an earlier generation was queued");
        assert!(mailroom.summary().contains("HELD: sent for an earlier launch"));
    }

    #[test]
    fn codex_mail_is_refused_and_held_after_the_bound_process_exits() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let count = root.path().join("count");
        let program = codex_stub(
            root.path(),
            "codex",
            &format!(
                "printf x >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' \"$3\"",
                count.display()
            ),
        );
        let mut router = NativeRouter::start(state_path(root.path()), Vec::new()).unwrap();
        router
            .bind_codex(vec![codex_entry(0, "Room A", Some(codex_room(&program, cwd.path(), "thread-a")))])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        router.send(&mut mailroom, "Sender", 0, "queued", None).unwrap();
        pump_for(&mut router, &mut mailroom, true, false, 300);

        // The room's process is gone: new mail is refused and saved mail is held.
        router.set_codex_live(0, false);
        let error = router
            .send(&mut mailroom, "Sender", 0, "after exit", None)
            .unwrap_err();
        assert!(error.contains("not running"), "{error}");
        pump_for(&mut router, &mut mailroom, false, false, 600);
        assert!(!count.exists(), "mail was queued into a dead room's thread");
        assert!(
            mailroom
                .summary()
                .contains("HELD: the Codex room process is no longer running"),
            "{}",
            mailroom.summary()
        );
    }

    const CLAUDE_A: &str = "1f8f8f8f-1111-4111-8111-111111111111";
    const CLAUDE_B: &str = "2f8f8f8f-2222-4222-8222-222222222222";

    /// A router with one bound Claude room, and the file its mod reads.
    fn claude_router(root: &Path, cwd: &Path) -> NativeRouter {
        let mut router = NativeRouter::start(state_path(root), Vec::new()).unwrap();
        router
            .claude_inbox(0)
            .unwrap()
            .attach(cwd, CLAUDE_A, 1)
            .unwrap();
        router
            .bind_claude(vec![ClaudeEntry {
                index: 0,
                title: "Claude · 1".to_owned(),
                session: Some(CLAUDE_A.to_owned()),
                generation: 1,
                reason: None,
            }])
            .unwrap();
        router
    }

    fn answered_with(router: &mut NativeRouter, mailroom: &mut Mailroom, log: &Log) -> NativeReply {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            router.tick(Instant::now(), mailroom, false, false);
            if let Some((_, outcome)) = log.borrow_mut().pop() {
                return outcome;
            }
            assert!(
                Instant::now() < deadline,
                "never answered:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A mod that polls in the background and receipts every message as started.
    fn polling(
        mod_: crate::claude_inbox::FakeMod,
    ) -> (Arc<AtomicBool>, thread::JoinHandle<crate::claude_inbox::FakeMod>) {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match mod_.next_claim() {
                    Some(claim) => {
                        mod_.receipt(&claim, "started", "");
                    }
                    None => thread::sleep(Duration::from_millis(20)),
                }
            }
            mod_
        });
        (stop, handle)
    }

    /// A Claude room's mail is held until its mod receipts it, accepted once it
    /// does, and held again when a reported session starts a new generation.
    #[test]
    fn a_claude_room_is_accepted_only_when_its_mod_receipts() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut router = claude_router(root.path(), cwd.path());
        let mut mailroom = Mailroom::new(50);

        let log = Log::default();
        router
            .send(&mut mailroom, "Sender", 0, "first", recorder(&log))
            .unwrap();
        assert_eq!(
            answered_with(&mut router, &mut mailroom, &log),
            NativeReply::Held(crate::native_outbox::CLAUDE_HOLD.to_owned())
        );

        // The mod reads the room's own connection file, registers and polls.
        let connection = crate::claude_inbox::mod_of(cwd.path(), CLAUDE_A);
        let registered = crate::claude_inbox::FakeMod::register(&connection.url, &connection.token);
        let (stop, mod_thread) = polling(registered);
        let log = Log::default();
        router
            .send(&mut mailroom, "Sender", 0, "second", recorder(&log))
            .unwrap();
        assert_eq!(
            answered_with(&mut router, &mut mailroom, &log),
            NativeReply::Accepted
        );
        let view = mailroom.summary();
        assert!(
            view.contains("ACCEPTED by Claude (submitted; not proof it was acted on)"),
            "{view}"
        );
        stop.store(true, Ordering::SeqCst);
        let old_mod = mod_thread.join().unwrap();

        // A reported session keeps the running mod's receiver: its process did
        // not restart, so it goes on polling, and only new-generation mail is
        // offered to it. The launch's connection file stays.
        router
            .rebind_claude(0, "Claude · 1".to_owned(), CLAUDE_B, 2)
            .unwrap();
        assert_eq!(
            old_mod.resumed().next_status(),
            200,
            "a report rebind keeps the receiver"
        );
        assert!(
            crate::claude_inbox::connection_file(cwd.path(), CLAUDE_A)
                .unwrap()
                .exists(),
            "the running process keeps its connection file"
        );
        let (stop, kept_mod) = polling(old_mod.resumed());
        let log = Log::default();
        router
            .send(&mut mailroom, "Sender", 0, "third", recorder(&log))
            .unwrap();
        assert_eq!(
            answered_with(&mut router, &mut mailroom, &log),
            NativeReply::Accepted,
            "the kept receiver collects new-generation mail"
        );
        stop.store(true, Ordering::SeqCst);
        kept_mod.join().unwrap();
    }
}
