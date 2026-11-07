//! Shared server state: the room index, rooms, members and rate limiters.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chattercrab_protocol::{
    CHAT_HISTORY_LEN, ChatMessage, DisplayName, PeerId, PeerInfo, RoomId, RoomName, RoomSummary,
    ServerMsg, UnixMillis,
};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use rand::RngExt;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;

/// Capacity of each connection's outbound message queue.
pub const OUTBOUND_QUEUE_CAPACITY: usize = 256;

/// Length of generated room ids.
const ROOM_ID_LEN: usize = 12;

/// Current wall-clock time in Unix milliseconds.
pub fn now_millis() -> UnixMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as UnixMillis)
        .unwrap_or_default()
}

/// State shared by every request handler and connection task.
pub struct AppState {
    /// Immutable configuration.
    pub config: Config,
    rooms: DashMap<RoomId, Arc<Room>>,
    room_count: AtomicUsize,
    next_room_seq: AtomicU64,
    create_limiter: DefaultKeyedRateLimiter<IpAddr>,
    ice_limiter: DefaultKeyedRateLimiter<IpAddr>,
    /// Cancelled when the server shuts down; parent of every member's kick token.
    pub shutdown: CancellationToken,
    /// Tracks upgraded WebSocket tasks so shutdown can wait for their close frames.
    pub tracker: TaskTracker,
}

/// Room creation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreateRoomError {
    /// `max_rooms` reached.
    #[error("the server already hosts the maximum number of rooms")]
    MaxRooms,
}

impl AppState {
    /// Creates empty state for `config`.
    pub fn new(config: Config, shutdown: CancellationToken) -> Self {
        let create_quota =
            Quota::per_minute(config.create_room_per_minute).allow_burst(config.create_room_burst);
        let ice_quota = Quota::per_minute(config.ice_per_minute);
        Self {
            config,
            rooms: DashMap::new(),
            room_count: AtomicUsize::new(0),
            next_room_seq: AtomicU64::new(0),
            create_limiter: RateLimiter::keyed(create_quota),
            ice_limiter: RateLimiter::keyed(ice_quota),
            shutdown,
            tracker: TaskTracker::new(),
        }
    }

    /// Returns true if `ip` may create a room now.
    pub fn allow_create_room(&self, ip: IpAddr) -> bool {
        self.create_limiter.check_key(&ip).is_ok()
    }

    /// Returns true if `ip` may fetch ICE servers now.
    pub fn allow_ice_servers(&self, ip: IpAddr) -> bool {
        self.ice_limiter.check_key(&ip).is_ok()
    }

    /// Creates a room with a fresh random id.
    pub fn create_room(&self, name: RoomName) -> Result<Arc<Room>, CreateRoomError> {
        let max = self.config.max_rooms;
        // Reserve a slot first so concurrent creations can never exceed `max_rooms`.
        let mut current = self.room_count.load(Ordering::Acquire);
        loop {
            if current >= max {
                return Err(CreateRoomError::MaxRooms);
            }
            match self.room_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        loop {
            let Ok(id) = RoomId::new(random_room_id()) else {
                continue;
            };
            if let Entry::Vacant(slot) = self.rooms.entry(id.clone()) {
                let seq = self.next_room_seq.fetch_add(1, Ordering::Relaxed);
                let room = Arc::new(Room::new(id, name, seq));
                slot.insert(Arc::clone(&room));
                return Ok(room);
            }
        }
    }

    /// Looks up a room. The map guard is released before returning.
    pub fn room(&self, id: &RoomId) -> Option<Arc<Room>> {
        self.rooms.get(id).map(|r| Arc::clone(r.value()))
    }

    /// Number of rooms currently hosted.
    pub fn room_count(&self) -> usize {
        self.room_count.load(Ordering::Acquire)
    }

    /// Summaries of every room in creation order.
    pub fn list_rooms(&self) -> Vec<RoomSummary> {
        let mut rooms: Vec<(u64, RoomSummary)> = self
            .rooms
            .iter()
            .map(|entry| (entry.value().seq, entry.value().summary()))
            .collect();
        // Creation sequence, not `created_at`: rooms created within the same
        // millisecond must still list in creation order.
        rooms.sort_unstable_by_key(|(seq, _)| *seq);
        rooms.into_iter().map(|(_, summary)| summary).collect()
    }

    /// Removes `room` from the index if it is still the registered instance.
    fn remove_room(&self, room: &Arc<Room>) {
        if self
            .rooms
            .remove_if(&room.id, |_, r| Arc::ptr_eq(r, room))
            .is_some()
        {
            self.room_count.fetch_sub(1, Ordering::AcqRel);
            tracing::info!(room = %room.id, "room deleted");
        }
    }

    /// Evicts empty rooms idle longer than the configured TTL and prunes rate-limiter
    /// state. Returns the number of evicted rooms.
    pub async fn sweep(&self) -> usize {
        let candidates: Vec<Arc<Room>> = self
            .rooms
            .iter()
            .filter(|e| e.value().member_count() == 0)
            .map(|e| Arc::clone(e.value()))
            .collect();
        let mut evicted = 0;
        for room in candidates {
            let mut inner = room.inner.lock().await;
            if inner.members.is_empty()
                && !inner.closed
                && inner.last_activity.elapsed() >= self.config.room_idle_ttl
            {
                inner.closed = true;
                drop(inner);
                self.remove_room(&room);
                evicted += 1;
            }
        }
        self.create_limiter.retain_recent();
        self.ice_limiter.retain_recent();
        evicted
    }
}

/// Random 12-character lowercase alphanumeric string (always a valid [`RoomId`]).
fn random_room_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..ROOM_ID_LEN)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect()
}

/// A room: immutable metadata plus mutable state behind its own lock.
pub struct Room {
    /// Room id.
    pub id: RoomId,
    /// Room name.
    pub name: RoomName,
    /// Creation time.
    pub created_at: UnixMillis,
    /// Server-wide creation sequence number, used for stable listing order.
    seq: u64,
    member_count: AtomicUsize,
    inner: Mutex<RoomInner>,
}

/// Mutable room state.
struct RoomInner {
    members: HashMap<PeerId, Member>,
    history: VecDeque<ChatMessage>,
    screen_publisher: Option<PeerId>,
    last_activity: Instant,
    next_chat_id: u64,
    next_join_seq: u64,
    closed: bool,
}

/// One joined connection.
struct Member {
    info: PeerInfo,
    tx: mpsc::Sender<ServerMsg>,
    kick: CancellationToken,
    join_seq: u64,
}

impl Member {
    /// Queues `msg`; a full queue kicks the member as a slow consumer.
    fn deliver(&self, msg: ServerMsg) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.kick.is_cancelled() {
                    tracing::warn!(peer = %self.info.id, "outbound queue full, dropping slow consumer");
                    self.kick.cancel();
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

impl RoomInner {
    fn broadcast(&self, msg: &ServerMsg, except: Option<PeerId>) {
        for (id, member) in &self.members {
            if Some(*id) != except {
                member.deliver(msg.clone());
            }
        }
    }
}

/// Why a join was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The room was deleted.
    #[error("room not found")]
    RoomNotFound,
    /// The room is at capacity.
    #[error("room is full")]
    RoomFull,
}

/// Result of a `screen_stop` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenStopOutcome {
    /// The lock was released and `screen_stopped` broadcast.
    Stopped,
    /// The requester was not the publisher.
    NotPublisher,
}

impl Room {
    fn new(id: RoomId, name: RoomName, seq: u64) -> Self {
        Self {
            id,
            name,
            created_at: now_millis(),
            seq,
            member_count: AtomicUsize::new(0),
            inner: Mutex::new(RoomInner {
                members: HashMap::new(),
                history: VecDeque::with_capacity(CHAT_HISTORY_LEN),
                screen_publisher: None,
                last_activity: Instant::now(),
                next_chat_id: 1,
                next_join_seq: 0,
                closed: false,
            }),
        }
    }

    /// Current number of members, without locking.
    pub fn member_count(&self) -> usize {
        self.member_count.load(Ordering::Acquire)
    }

    /// REST summary of this room.
    pub fn summary(&self) -> RoomSummary {
        RoomSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            member_count: self.member_count(),
            created_at: self.created_at,
        }
    }

    /// Adds a member. On success `welcome` has been queued on `tx` and `peer_joined`
    /// broadcast to the other members.
    pub async fn join(
        &self,
        display_name: DisplayName,
        tx: mpsc::Sender<ServerMsg>,
        kick: CancellationToken,
        max_size: usize,
    ) -> Result<PeerId, JoinError> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(JoinError::RoomNotFound);
        }
        if inner.members.len() >= max_size {
            return Err(JoinError::RoomFull);
        }
        let mut id = PeerId::random();
        while inner.members.contains_key(&id) {
            id = PeerId::random();
        }
        let info = PeerInfo { id, display_name };

        let mut existing: Vec<&Member> = inner.members.values().collect();
        existing.sort_by_key(|m| m.join_seq);
        let welcome = ServerMsg::Welcome {
            you: id,
            peers: existing.iter().map(|m| m.info.clone()).collect(),
            history: inner.history.iter().cloned().collect(),
            screen_publisher: inner.screen_publisher,
        };
        // The queue is fresh and empty, so this cannot fail for capacity reasons.
        let _ = tx.try_send(welcome);
        inner.broadcast(&ServerMsg::PeerJoined { peer: info.clone() }, None);

        let join_seq = inner.next_join_seq;
        inner.next_join_seq += 1;
        inner.members.insert(
            id,
            Member {
                info,
                tx,
                kick,
                join_seq,
            },
        );
        inner.last_activity = Instant::now();
        self.member_count
            .store(inner.members.len(), Ordering::Release);
        Ok(id)
    }

    /// Removes a member, broadcasts `peer_left` (and `screen_stopped` if it was
    /// publishing) and deletes the room when it becomes empty.
    pub async fn leave(self: &Arc<Self>, state: &AppState, me: PeerId) {
        let mut inner = self.inner.lock().await;
        if inner.members.remove(&me).is_none() {
            return;
        }
        self.member_count
            .store(inner.members.len(), Ordering::Release);
        inner.last_activity = Instant::now();
        inner.broadcast(&ServerMsg::PeerLeft { peer_id: me }, None);
        if inner.screen_publisher == Some(me) {
            inner.screen_publisher = None;
            inner.broadcast(&ServerMsg::ScreenStopped, None);
        }
        if inner.members.is_empty() {
            inner.closed = true;
            drop(inner);
            state.remove_room(self);
        }
    }

    /// Delivers `msg` to member `to`. Returns false if `to` is not in the room.
    pub async fn relay(&self, to: PeerId, msg: ServerMsg) -> bool {
        let mut inner = self.inner.lock().await;
        inner.last_activity = Instant::now();
        match inner.members.get(&to) {
            Some(member) => {
                member.deliver(msg);
                true
            }
            None => false,
        }
    }

    /// Appends a chat message to history and broadcasts it to every member.
    /// `text` must already be validated.
    pub async fn chat(&self, from: PeerId, text: String) {
        let mut inner = self.inner.lock().await;
        let Some(display_name) = inner
            .members
            .get(&from)
            .map(|m| m.info.display_name.clone())
        else {
            return;
        };
        let msg = ChatMessage {
            id: inner.next_chat_id,
            from,
            display_name,
            text,
            ts: now_millis(),
        };
        inner.next_chat_id += 1;
        if inner.history.len() == CHAT_HISTORY_LEN {
            inner.history.pop_front();
        }
        inner.history.push_back(msg.clone());
        inner.last_activity = Instant::now();
        inner.broadcast(&ServerMsg::Chat { msg }, None);
    }

    /// Handles `screen_request`; returns the reply for the requester.
    pub async fn screen_request(&self, me: PeerId) -> ServerMsg {
        let mut inner = self.inner.lock().await;
        inner.last_activity = Instant::now();
        match inner.screen_publisher {
            None => {
                inner.screen_publisher = Some(me);
                inner.broadcast(&ServerMsg::ScreenStarted { publisher: me }, Some(me));
                // The requester gets `screen_granted` first, then `screen_started`
                // through its queue, preserving the documented order.
                if let Some(member) = inner.members.get(&me) {
                    member.deliver(ServerMsg::ScreenStarted { publisher: me });
                }
                ServerMsg::ScreenGranted
            }
            Some(publisher) if publisher == me => ServerMsg::ScreenGranted,
            Some(publisher) => {
                let who = inner
                    .members
                    .get(&publisher)
                    .map(|m| m.info.display_name.to_string())
                    .unwrap_or_else(|| "another peer".to_owned());
                ServerMsg::ScreenDenied {
                    reason: format!("{who} is already sharing"),
                }
            }
        }
    }

    /// Handles `screen_stop`.
    pub async fn screen_stop(&self, me: PeerId) -> ScreenStopOutcome {
        let mut inner = self.inner.lock().await;
        inner.last_activity = Instant::now();
        if inner.screen_publisher != Some(me) {
            return ScreenStopOutcome::NotPublisher;
        }
        inner.screen_publisher = None;
        inner.broadcast(&ServerMsg::ScreenStopped, None);
        ScreenStopOutcome::Stopped
    }
}
