//! UI-side model of the current room, updated from [`AppEvent`]s. Pure and testable.

use std::collections::VecDeque;

use chattercrab_media::PeerConnectionState;
use chattercrab_protocol::{ChatMessage, PeerId, PeerInfo, RoomId, RoomName};

use crate::net::{AppEvent, JoinedRoom};

/// RMS level above which a member is shown as speaking.
pub const SPEAKING_THRESHOLD: f32 = 0.02;
/// Chat lines kept in memory for display (never written to disk).
pub const MAX_CHAT_LINES: usize = 1000;
/// Default per-peer playback volume.
pub const DEFAULT_VOLUME: f32 = 1.0;

/// One remote member as displayed.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberView {
    /// Identity.
    pub info: PeerInfo,
    /// Last reported audio level.
    pub level: f32,
    /// Playback volume (0.0 to 5.0), kept while locally muted.
    pub volume: f32,
    /// Locally muted (volume 0 sent to the media engine).
    pub muted: bool,
    /// Connection state, if reported yet.
    pub state: Option<PeerConnectionState>,
}

impl MemberView {
    fn new(info: PeerInfo) -> Self {
        Self {
            info,
            level: 0.0,
            volume: DEFAULT_VOLUME,
            muted: false,
            state: None,
        }
    }

    /// Whether the member is currently speaking.
    pub fn speaking(&self) -> bool {
        self.level >= SPEAKING_THRESHOLD
    }

    /// Volume the media engine should apply.
    pub fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }
}

/// Everything the room screen displays.
#[derive(Debug, Clone)]
pub struct RoomModel {
    /// Room id.
    pub id: RoomId,
    /// Room name.
    pub name: RoomName,
    /// Our peer id, once joined.
    pub me: Option<PeerId>,
    /// Our display name.
    pub my_name: String,
    /// Remote members in join order.
    pub members: Vec<MemberView>,
    /// Chat log, oldest first.
    pub chat: VecDeque<ChatMessage>,
    /// Current screen publisher.
    pub screen_publisher: Option<PeerId>,
    /// Whether we publish our screen.
    pub local_sharing: bool,
    /// Our microphone level.
    pub local_level: f32,
    /// Microphone muted.
    pub muted: bool,
    /// Playback deafened.
    pub deafened: bool,
    /// Reconnect status line, while signaling is down.
    pub reconnecting: Option<String>,
}

impl RoomModel {
    /// A room we are joining (before `welcome`).
    pub fn joining(id: RoomId, name: RoomName, my_name: String) -> Self {
        Self {
            id,
            name,
            me: None,
            my_name,
            members: Vec::new(),
            chat: VecDeque::new(),
            screen_publisher: None,
            local_sharing: false,
            local_level: 0.0,
            muted: false,
            deafened: false,
            reconnecting: None,
        }
    }

    /// Applies a joined or re-joined state. Peer ids change on every join, so
    /// per-peer volume settings start fresh.
    pub fn apply_joined(&mut self, joined: JoinedRoom) {
        self.id = joined.id;
        self.name = joined.name;
        self.me = Some(joined.me);
        self.my_name = joined.display_name.to_string();
        self.members = joined.peers.into_iter().map(MemberView::new).collect();
        self.chat = joined.history.into();
        self.trim_chat();
        self.screen_publisher = joined.screen_publisher;
        self.local_sharing = false;
        self.reconnecting = None;
    }

    fn trim_chat(&mut self) {
        while self.chat.len() > MAX_CHAT_LINES {
            self.chat.pop_front();
        }
    }

    /// Display name of a member (or ourselves).
    pub fn name_of(&self, peer: PeerId) -> Option<&str> {
        if Some(peer) == self.me {
            return Some(&self.my_name);
        }
        self.members
            .iter()
            .find(|m| m.info.id == peer)
            .map(|m| m.info.display_name.as_str())
    }

    /// Mutable access to a member.
    pub fn member_mut(&mut self, peer: PeerId) -> Option<&mut MemberView> {
        self.members.iter_mut().find(|m| m.info.id == peer)
    }

    /// Whether a remote screen share should be displayed.
    pub fn remote_share_active(&self) -> bool {
        matches!(self.screen_publisher, Some(p) if Some(p) != self.me)
    }

    /// Updates the model; returns `true` if the event was relevant to the room.
    pub fn apply(&mut self, event: &AppEvent) -> bool {
        match event {
            AppEvent::Joined(joined) => self.apply_joined(joined.clone()),
            AppEvent::PeerJoined(info) => {
                if self.member_mut(info.id).is_none() {
                    self.members.push(MemberView::new(info.clone()));
                }
            }
            AppEvent::PeerLeft(peer) => self.members.retain(|m| m.info.id != *peer),
            AppEvent::Chat(msg) => {
                self.chat.push_back(msg.clone());
                self.trim_chat();
            }
            AppEvent::ScreenStarted(peer) => self.screen_publisher = Some(*peer),
            AppEvent::ScreenStopped => self.screen_publisher = None,
            AppEvent::LocalShare(active) => self.local_sharing = *active,
            AppEvent::PeerState { peer, state } => {
                if let Some(m) = self.member_mut(*peer) {
                    m.state = Some(*state);
                }
            }
            AppEvent::Levels { local, peers } => {
                self.local_level = *local;
                for (peer, level) in peers {
                    if let Some(m) = self.member_mut(*peer) {
                        m.level = *level;
                    }
                }
            }
            AppEvent::Reconnecting {
                attempt,
                delay_secs,
            } => {
                self.members.clear();
                self.me = None;
                self.screen_publisher = None;
                self.local_sharing = false;
                self.reconnecting = Some(format!(
                    "Connection lost. Reconnecting in {delay_secs} s (attempt {attempt})."
                ));
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chattercrab_protocol::DisplayName;

    fn peer(n: u8) -> PeerId {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn info(n: u8) -> PeerInfo {
        PeerInfo {
            id: peer(n),
            display_name: DisplayName::new(&format!("p{n}")).unwrap(),
        }
    }

    fn chat(id: u64, from: u8) -> ChatMessage {
        ChatMessage {
            id,
            from: peer(from),
            display_name: DisplayName::new(&format!("p{from}")).unwrap(),
            text: format!("m{id}"),
            ts: id,
        }
    }

    fn joined_model() -> RoomModel {
        let mut m = RoomModel::joining(
            RoomId::new("r").unwrap(),
            RoomName::new("R").unwrap(),
            "me".into(),
        );
        m.apply(&AppEvent::Joined(JoinedRoom {
            id: RoomId::new("r").unwrap(),
            name: RoomName::new("R").unwrap(),
            me: peer(1),
            display_name: DisplayName::new("me").unwrap(),
            peers: vec![info(2)],
            history: vec![chat(1, 2)],
            screen_publisher: None,
        }));
        m
    }

    #[test]
    fn join_and_membership() {
        let mut m = joined_model();
        assert_eq!(m.members.len(), 1);
        assert_eq!(m.chat.len(), 1);
        m.apply(&AppEvent::PeerJoined(info(3)));
        m.apply(&AppEvent::PeerJoined(info(3)));
        assert_eq!(m.members.len(), 2);
        m.apply(&AppEvent::PeerLeft(peer(2)));
        assert_eq!(m.members[0].info.id, peer(3));
        assert_eq!(m.name_of(peer(1)), Some("me"));
        assert_eq!(m.name_of(peer(3)), Some("p3"));
    }

    #[test]
    fn chat_appends_and_is_bounded() {
        let mut m = joined_model();
        m.apply(&AppEvent::Chat(chat(2, 1)));
        assert_eq!(m.chat.back().unwrap().text, "m2");
        for i in 0..(MAX_CHAT_LINES as u64 + 10) {
            m.apply(&AppEvent::Chat(chat(10 + i, 2)));
        }
        assert_eq!(m.chat.len(), MAX_CHAT_LINES);
        assert_eq!(m.chat.back().unwrap().id, 10 + MAX_CHAT_LINES as u64 + 9);
    }

    #[test]
    fn screen_state_transitions() {
        let mut m = joined_model();
        m.apply(&AppEvent::ScreenStarted(peer(2)));
        assert!(m.remote_share_active());
        m.apply(&AppEvent::ScreenStopped);
        assert!(!m.remote_share_active());
        // Our own share is not displayed as a remote stream.
        m.apply(&AppEvent::ScreenStarted(peer(1)));
        m.apply(&AppEvent::LocalShare(true));
        assert!(!m.remote_share_active());
        assert!(m.local_sharing);
        m.apply(&AppEvent::LocalShare(false));
        assert!(!m.local_sharing);
    }

    #[test]
    fn levels_speaking_and_volume() {
        let mut m = joined_model();
        m.apply(&AppEvent::Levels {
            local: 0.3,
            peers: vec![(peer(2), 0.05)],
        });
        assert!(m.members[0].speaking());
        assert_eq!(m.local_level, 0.3);
        m.apply(&AppEvent::Levels {
            local: 0.0,
            peers: vec![(peer(2), 0.001)],
        });
        assert!(!m.members[0].speaking());
        let member = m.member_mut(peer(2)).unwrap();
        member.volume = 1.5;
        member.muted = true;
        assert_eq!(member.effective_volume(), 0.0);
        member.muted = false;
        assert_eq!(member.effective_volume(), 1.5);
    }

    #[test]
    fn reconnect_clears_mesh_until_welcome() {
        let mut m = joined_model();
        m.apply(&AppEvent::Reconnecting {
            attempt: 1,
            delay_secs: 1,
        });
        assert!(m.members.is_empty());
        assert!(m.reconnecting.is_some());
        // Chat log stays visible while reconnecting.
        assert_eq!(m.chat.len(), 1);
        m.apply(&AppEvent::Joined(JoinedRoom {
            id: RoomId::new("r").unwrap(),
            name: RoomName::new("R").unwrap(),
            me: peer(9),
            display_name: DisplayName::new("me").unwrap(),
            peers: vec![info(2), info(3)],
            history: vec![],
            screen_publisher: Some(peer(3)),
        }));
        assert!(m.reconnecting.is_none());
        assert_eq!(m.members.len(), 2);
        assert!(m.remote_share_active());
    }
}
