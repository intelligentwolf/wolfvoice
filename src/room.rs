/*
 * WolfVoice - spatial WebRTC voice for OpenSimulator
 * By Shadow Wolf and Lone Wolf, Wolf Territories Grid and IntelligentWolf Ltd
 * Copyright 2026 IntelligentWolf Ltd
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Rooms, sessions and the shared registry.
//!
//! A "session" is one viewer peer connection. Firestorm opens one per region it
//! can hear (its own plus any WebRTC-enabled neighbour — llvoicewebrtc.cpp:2237-2250),
//! so one agent may legitimately hold several sessions at once. Only the one whose
//! region is the agent's current region reports itself as primary via the join
//! message (llvoicewebrtc.cpp:3304-3307), and the viewer deliberately ignores joins
//! announced by non-primary servers (:3179-3186) so a neighbour cannot duplicate a
//! participant.

use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use crate::mixer::{self, FRAME_SAMPLES};
use crate::proto::{self, RosterEntry};
use crate::truth::{self, Truths};

/// How many decoded frames we are willing to hold per speaker before dropping the
/// oldest. 10 frames == 200 ms; past that the audio is stale enough that keeping it
/// only adds delay.
const JITTER_MAX_FRAMES: usize = 10;

/// Which room a session belongs to.
///
/// Spatial ("local") voice is keyed on region + parcel because that is exactly what
/// the viewer asks for: one connection per region, carrying parcel_local_id only
/// when parcel voice is in play (llvoicewebrtc.cpp:2796-2799). Group and IM voice
/// ("multiagent") is keyed on the channel id the viewer supplies (:3400-3401).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RoomKey {
    Spatial { region: String, parcel: i32 },
    MultiAgent { channel: String },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpatialState {
    pub avatar_pos: [f64; 3],
    pub avatar_rot: [f64; 4],
    pub listener_pos: [f64; 3],
    pub listener_rot: [f64; 4],
    /// False until the viewer has actually sent a position. Until then the session
    /// must not be placed at the origin — that would make every silent newcomer
    /// audible to everyone standing near <0,0,0>.
    pub known: bool,
}

impl SpatialState {
    fn identity() -> Self {
        SpatialState {
            avatar_rot: [0.0, 0.0, 0.0, 1.0],
            listener_rot: [0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        }
    }
}

/// What one listener has already been told about one speaker.
///
/// Source: llvoicewebrtc.cpp:3181-3209 OnDataReceivedImpl. The viewer creates a
/// participant only inside
///     if (!participant && joined && (primary || !isSpatial()))
/// where `joined` means the entry carried a `"j"` object and `primary` is that
/// object's `"p"` member. So for a spatial channel a join announced with
/// `"p":false` is silently discarded and there is no second chance — nothing else
/// in the protocol ever creates a participant.
///
/// The viewer's own primary flag is not settled when it first announces itself: a
/// spatial connection is constructed non-primary (llvoicewebrtc.cpp:3322), sends
/// its first join in that state (:2985), and only re-sends with `"p":true` once
/// VOICE_STATE_SESSION_UP notices the connection belongs to the agent's own
/// region (:3010-3017). Remembering the flag we last announced is what lets us
/// re-send the join when it flips.
///
/// `power` is remembered for the same reason in the other direction: the viewer
/// keeps `mLevel`/`mIsSpeaking` at whatever we last sent (:3222-3238), so a
/// speaker who falls silent must be reported at zero once or their indicator
/// stays lit for ever.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Announced {
    primary: bool,
    power: u8,
}

/// Per-session mutable audio and control state.
///
/// The peer connection itself is deliberately NOT stored here: it is owned by the
/// task that created it, and everything the mixer needs (the outbound track, the
/// decoded frames) is reachable through this struct. That keeps the mixer free of
/// any lock that a network callback also wants.
pub struct Session {
    pub id: String,
    pub agent_id: String,
    /// The room this session is mixed in. Behind a lock because a session can MOVE
    /// (`Registry::relocate`, the Wolf "wolf_move" provision): same peer connection,
    /// same media, a different region's room.
    room: RwLock<RoomKey>,
    pub spatial: bool,

    pub state: RwLock<SpatialState>,
    /// Listener-chosen gain per speaker, already converted out of the viewer's
    /// PEER_GAIN_CONVERSION_FACTOR units.
    pub gains: RwLock<HashMap<String, f32>>,
    pub mutes: RwLock<HashMap<String, bool>>,

    /// Decoded 20 ms mono frames from THIS session's microphone.
    pub inbound: Mutex<VecDeque<Vec<f32>>>,
    /// Most recent speech level of this session, on the viewer's 0-127 scale.
    pub level: AtomicU8,

    /// The viewer has sent its join ("j") on this channel since it opened, or since its last
    /// wolf_move. Both directions hang on it: others are told about this agent only once it has
    /// joined, and this listener is sent a roster only once it has joined (build_roster).
    pub joined: AtomicBool,
    pub primary: AtomicBool,
    pub closed: AtomicBool,

    /// True only while this session's "SLData" channel is open, i.e. while a
    /// roster we build can actually be delivered. It is false for the second or
    /// so between the session being registered and the viewer completing ICE and
    /// DTLS, and the mixer runs 50 times a second in that window.
    pub ready: AtomicBool,

    /// True once this session has reported an avatar position. Used only for the
    /// one-shot log that records whether positions ever arrived; the mixer reads
    /// `SpatialState::known`.
    position_logged: AtomicBool,

    /// True once the viewer has trickled us a usable ICE candidate.
    ///
    /// The viewer can lose its own candidates before they ever reach us: it posts
    /// them to VoiceSignalingRequest as soon as any have been gathered
    /// (llvoicewebrtc.cpp:2898 calls processIceUpdates on every 100 ms tick, from
    /// the first state onwards) and clears its list when it does (:2508), even
    /// though `viewer_session` (:2518) is still empty until our provision reply
    /// arrives. OpenSim then cannot resolve the empty id and drops the
    /// request outright — WebRtcVoiceServiceModule.cs:255-262 logs "viewer session
    /// not found" and returns null, so we never see those candidates. Recording
    /// whether ANY arrived is what separates "the viewer never told us where it
    /// is" from "we could not reach it".
    pub received_candidate: AtomicBool,

    /// [HARDENING 2026-10-23] Remote ICE candidates handed to the transport so far, trickled or
    /// in the offer; capped at ice::MAX_CANDIDATES_PER_SESSION (main.rs signaling).
    pub remote_candidates: AtomicU32,

    /// When the session was made: one whose data channel never opened is reaped after
    /// UNREADY_REAP (main.rs mixer loop), so half-open sessions cannot pile up (scan M5).
    pub created: std::time::Instant,

    /// [2026-10-22] Why the session ended, for the one "removed" journal line: the first cause recorded wins
    /// (viewer logout, re-provision, ICE failure, data channel closed, never connected).
    end_reason: Mutex<&'static str>,
    /// [2026-10-22] The "data channel open but no join yet" warning has been logged (once per session).
    join_warned: AtomicBool,

    /// What this listener currently believes about each speaker, so we send an
    /// update exactly when that belief goes stale.
    announced: Mutex<HashMap<String, Announced>>,
}

impl Session {
    pub fn new(id: String, agent_id: String, room: RoomKey, spatial: bool) -> Self {
        Session {
            id,
            agent_id,
            room: RwLock::new(room),
            spatial,
            state: RwLock::new(SpatialState::identity()),
            gains: RwLock::new(HashMap::new()),
            mutes: RwLock::new(HashMap::new()),
            inbound: Mutex::new(VecDeque::new()),
            level: AtomicU8::new(0),
            joined: AtomicBool::new(false),
            primary: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            created: std::time::Instant::now(),
            position_logged: AtomicBool::new(false),
            received_candidate: AtomicBool::new(false),
            remote_candidates: AtomicU32::new(0),
            end_reason: Mutex::new(""),
            join_warned: AtomicBool::new(false),
            announced: Mutex::new(HashMap::new()),
        }
    }

    /// [2026-10-22] Record why this session is ending; the first cause recorded is kept.
    pub fn set_end_reason(&self, reason: &'static str) {
        let mut r = self.end_reason.lock();
        if r.is_empty() {
            *r = reason;
        }
    }

    /// The recorded end cause, or "unknown".
    pub fn end_reason(&self) -> &'static str {
        let r = *self.end_reason.lock();
        if r.is_empty() { "unknown" } else { r }
    }

    /// [2026-10-22] True exactly once for a session whose data channel has been open for `after` without the
    /// viewer's join - the viewer then never shows anyone in the room and sends nothing on it (wolfvoice journal
    /// 10-22 04:13:00, session dac59c09).
    pub fn join_overdue(&self, after: std::time::Duration) -> bool {
        self.ready.load(Ordering::Acquire)
            && !self.joined.load(Ordering::Acquire)
            && self.created.elapsed() > after
            && !self.join_warned.swap(true, Ordering::AcqRel)
    }

    /// The room this session is mixed in now.
    pub fn room(&self) -> RoomKey {
        self.room.read().clone()
    }

    /// Record whether the data channel is open.
    ///
    /// Going closed also forgets everything we believe this listener has been
    /// told: nothing more can be delivered, and if the channel ever comes back
    /// the viewer will have a fresh, empty participant list that has to be
    /// rebuilt from joins.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
        if !ready {
            self.forget_all_announced();
        }
    }

    /// Drop every belief about what this listener has been told, so the next tick
    /// re-announces from scratch. Called when a roster could not be delivered.
    pub fn forget_all_announced(&self) {
        self.announced.lock().clear();
    }

    /// Push one decoded frame, dropping the oldest if the listener is not keeping up.
    pub fn push_frame(&self, frame: Vec<f32>) {
        let mut q = self.inbound.lock();
        if q.len() >= JITTER_MAX_FRAMES {
            q.pop_front();
        }
        q.push_back(frame);
    }

    /// Take the next frame for this tick, or None for silence.
    pub fn take_frame(&self) -> Option<Vec<f32>> {
        self.inbound.lock().pop_front()
    }

    /// Apply a control message that arrived on the data channel.
    pub fn apply_data(&self, m: &proto::ViewerDataMessage) {
        if let Some(primary) = m.join {
            self.primary.store(primary, Ordering::Relaxed);
            // The first join on this channel (or the first since a wolf_move) is the viewer telling
            // us it is now listening, which is what opens this session's roster (see build_roster).
            // Anything recorded as told before it was told to a viewer that could not hear it, so
            // start the participant list from scratch.
            // Source: llvoicewebrtc.cpp:3911-3926 OnDataChannelReady registers the data observer
            // and sets mWebRTCDataInterface in one main-queue task; sendJoin (:3935-3952) needs
            // mWebRTCDataInterface, so a join can only be sent once the observer is registered.
            if !self.joined.swap(true, Ordering::AcqRel) {
                self.forget_all_announced();
                log::info!(
                    "session {} agent {} joined (primary={primary}); its participant roster starts now",
                    self.id,
                    self.agent_id
                );
            }
        }

        // Reject anything not finite before it reaches the mixer. JSON can carry
        // 1e308, and squaring that gives inf; inf/inf in the panning maths is NaN,
        // which would then be summed into the mix and handed to the Opus encoder.
        let finite3 = |v: Option<[f64; 3]>| v.filter(|a| a.iter().all(|c| c.is_finite()));
        let finite4 = |v: Option<[f64; 4]>| v.filter(|a| a.iter().all(|c| c.is_finite()));
        let m = &proto::ViewerDataMessage {
            join: m.join,
            avatar_pos: finite3(m.avatar_pos),
            avatar_rot: finite4(m.avatar_rot),
            listener_pos: finite3(m.listener_pos),
            listener_rot: finite4(m.listener_rot),
            user_gain: m.user_gain.clone(),
            user_mute: m.user_mute.clone(),
        };

        if m.avatar_pos.is_some()
            || m.avatar_rot.is_some()
            || m.listener_pos.is_some()
            || m.listener_rot.is_some()
        {
            let mut s = self.state.write();
            if let Some(p) = m.avatar_pos {
                s.avatar_pos = p;
                s.known = true;
            }
            if let Some(r) = m.avatar_rot {
                s.avatar_rot = r;
            }
            if let Some(p) = m.listener_pos {
                s.listener_pos = p;
            }
            if let Some(r) = m.listener_rot {
                s.listener_rot = r;
            }
            // Re-apply the 50 m tether server-side. llvoicewebrtc.cpp:1209-1212 is
            // explicit that this must be enforced here and not merely in the client:
            // a modified viewer could otherwise move its ear anywhere.
            if s.known {
                s.listener_pos = mixer::tether(s.listener_pos, s.avatar_pos);
            }
        }

        // A spatial speaker with no reported position is dropped from the mix
        // (see `mix_room`), which looks exactly like a broken media path. Record
        // the arrival once, at info level, so the journal answers "did positions
        // ever turn up?" without having to raise the log level on a live server.
        let known = self.state.read().known;
        if known && !self.position_logged.swap(true, Ordering::Relaxed) {
            log::info!(
                "session {} agent {} reported its first position",
                self.id,
                self.agent_id
            );
        }

        if !m.user_gain.is_empty() {
            let mut g = self.gains.write();
            for (id, raw) in &m.user_gain {
                // Clamped: a raw u32 divides down to a gain of ~19 million, and the
                // map is bounded so a client cannot grow it without limit.
                let gain = (*raw as f32 / proto::PEER_GAIN_CONVERSION_FACTOR)
                    .clamp(0.0, proto::MAX_PEER_GAIN);
                if g.len() < proto::MAX_PEER_ENTRIES || g.contains_key(id) {
                    g.insert(id.clone(), gain);
                }
            }
        }
        if !m.user_mute.is_empty() {
            let mut mu = self.mutes.write();
            for (id, muted) in &m.user_mute {
                if mu.len() < proto::MAX_PEER_ENTRIES || mu.contains_key(id) {
                    mu.insert(id.clone(), *muted);
                }
            }
        }
    }

    /// Gain this listener wants applied to `speaker`, combining their explicit
    /// volume choice with their mute list. Defaults to unity for an unknown peer.
    pub fn peer_gain(&self, speaker: &str) -> f32 {
        if *self.mutes.read().get(speaker).unwrap_or(&false) {
            return 0.0;
        }
        *self.gains.read().get(speaker).unwrap_or(&1.0)
    }

    fn forget_announced(&self, agent: &str) {
        self.announced.lock().remove(agent);
    }

    /// True once this session has ever reported a position.
    pub fn reported_position(&self) -> bool {
        self.state.read().known
    }
}

/// All live sessions, indexed both ways.
#[derive(Default)]
pub struct Registry {
    by_session: RwLock<HashMap<String, Arc<Session>>>,
    by_room: RwLock<HashMap<RoomKey, Vec<String>>>,
}

impl Registry {
    pub fn insert(&self, s: Arc<Session>) {
        self.by_room
            .write()
            .entry(s.room())
            .or_default()
            .push(s.id.clone());
        self.by_session.write().insert(s.id.clone(), s);
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        let s = self.by_session.write().remove(id)?;
        let mut rooms = self.by_room.write();
        let key = s.room();
        if let Some(members) = rooms.get_mut(&key) {
            members.retain(|m| m != id);
            if members.is_empty() {
                rooms.remove(&key);
            }
        }
        // Everyone who had been told about this agent should hear about them again
        // if they come back.
        drop(rooms);
        for other in self.by_session.read().values() {
            other.forget_announced(&s.agent_id);
        }
        s.closed.store(true, Ordering::Relaxed);
        Some(s)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.by_session.read().get(id).cloned()
    }

    /// Move a live session to another room - the Wolf "wolf_move". Nothing about the
    /// transport changes; the mixer simply finds it in the new room from the next tick.
    ///
    /// Returns the room it left, or None when there is no such session or it is
    /// already there. Beliefs are reset on both sides so the rosters come out right:
    /// the mover forgets everything it was told (the new room's people are announced
    /// to it as joins), and everyone in the old room forgets the mover (so if they
    /// meet again they are told again).
    pub fn relocate(&self, id: &str, to: RoomKey) -> Option<RoomKey> {
        // Lock order by_session -> by_room, the same as `remove`, so the two can never deadlock;
        // holding by_session also means the session cannot be removed half-way through a move
        // (and pushed back into a room it has left - security scan, Low).
        let sessions = self.by_session.read();
        let s = sessions.get(id)?.clone();
        let from = {
            let mut rooms = self.by_room.write();
            let mut key = s.room.write();
            if *key == to {
                return None;
            }
            // Silence the mover's roster before it appears in the new room (and only once a move
            // is certain, so a no-op move cannot leave it gated), so no mixer tick can build the
            // new room's joins for it until it joins again; the mixer reads room membership under
            // by_room, which we hold. Until then the viewer still files what it receives under the
            // OLD channel and region: llvoicewebrtc.cpp wolfRequestMove re-keys mChannelID /
            // mRegionID only when our reply arrives through the region (:4050-4052), while a
            // roster on the data channel would arrive first and be filed under the old session,
            // which wolfOnParcelMoveResult then shuts down (:1369-1373). The viewer sends its join
            // again once it has re-keyed (wolfRejoinAfterMove :3213-3223; WolfStorm
            // voice_llwebrtc.js:1026), and apply_data reopens the roster from scratch on it.
            s.joined.store(false, Ordering::Release);
            let from = key.clone();
            if let Some(members) = rooms.get_mut(&from) {
                members.retain(|m| m != id);
                if members.is_empty() {
                    rooms.remove(&from);
                }
            }
            rooms.entry(to.clone()).or_default().push(id.to_string());
            *key = to;
            from
        };
        drop(sessions);
        s.forget_all_announced();
        for other in self.members(&from) {
            other.forget_announced(&s.agent_id);
        }
        Some(from)
    }

    pub fn rooms(&self) -> Vec<RoomKey> {
        self.by_room.read().keys().cloned().collect()
    }

    pub fn members(&self, room: &RoomKey) -> Vec<Arc<Session>> {
        let ids = match self.by_room.read().get(room) {
            Some(v) => v.clone(),
            None => return Vec::new(),
        };
        let map = self.by_session.read();
        ids.iter().filter_map(|id| map.get(id).cloned()).collect()
    }

    pub fn session_count(&self) -> usize {
        self.by_session.read().len()
    }

    /// How many sessions this agent holds (neighbour regions + group/IM calls).
    pub fn sessions_of(&self, agent: &str) -> usize {
        self.by_session.read().values().filter(|s| s.agent_id == agent).count()
    }

    /// [HARDENING 2026-10-23] Every agent holding at least one spatial session: the agents
    /// whose region positions the mixer uses (main.rs positions answers regions with them).
    pub fn spatial_agents(&self) -> std::collections::HashSet<String> {
        self.by_session.read().values().filter(|s| s.spatial).map(|s| s.agent_id.clone()).collect()
    }
}

/// What one listener should be sent this tick: the mixed stereo frame and any
/// roster changes.
pub struct ListenerOutput {
    pub session: Arc<Session>,
    pub stereo: Vec<f32>,
    pub roster: Option<String>,
}

/// Compute one 20 ms tick for a whole room.
///
/// Pulled out of the network layer so it is testable without any WebRTC at all.
#[cfg_attr(not(test), allow(dead_code))]
pub fn mix_room(members: &[Arc<Session>]) -> Vec<ListenerOutput> {
    mix_room_with(members, &[], &Truths::default())
}

/// [HARDENING 2026-10-23] Where a session is mixed from this tick: what its viewer reported
/// (state), held to its region's position when the region has sent a fresh one
/// (src/truth.rs). The avatar is pulled to within the tolerance of the region's position, and
/// the ear is tethered to 50 m of the region's position, not of the avatar the viewer claims
/// (llvoicewebrtc.cpp:1185-1188: the tether "will also be enforced on the voice server based
/// on position sent from the simulator").
fn placed(s: &Session, truths: &Truths, now: std::time::Instant) -> SpatialState {
    let mut st = *s.state.read();
    if s.spatial && st.known {
        if let Some(t) = truths.fresh(&s.agent_id, now) {
            st.avatar_pos = truth::clamp_near(st.avatar_pos, t.pos, t.tolerance);
            st.listener_pos = mixer::tether(st.listener_pos, t.pos);
        }
    }
    st
}

/// One member's part in this tick: its frame (taken once and reused for every listener) and
/// where it is mixed from.
pub struct TickMember {
    session: Arc<Session>,
    frame: Option<Vec<f32>>,
    placed: SpatialState,
}

/// One tick with story characters speaking too ([WOLF ADVENTURE P2], src/npc.rs). `npcs` are
/// the frames of characters on THIS room's region; each is mixed for every spatial listener
/// exactly like a real speaker at its position (distance gain, pan, the listener's own gain and
/// mute for that uuid), limited to its `listeners` when it names them. Never in the roster.
///
/// [HARDENING 2026-10-23] A session takes part in the audio only once its viewer has JOINED
/// (sent "j" on the data channel): before that it hears nothing and nobody hears it. Before
/// this a session with an open data channel that never joined got the room's mix - a listener
/// nobody was told about, since the roster also waits for the join. Stock Firestorm always
/// joins as soon as its data channel is ready (llvoicewebrtc.cpp:2983-2986 sendJoin), so it
/// loses nothing.
pub fn mix_room_with(members: &[Arc<Session>], npcs: &[Arc<crate::npc::NpcFrame>], truths: &Truths) -> Vec<ListenerOutput> {
    let now = std::time::Instant::now();
    // Pull exactly one frame per speaker, once, and reuse it for every listener.
    let mut frames: Vec<TickMember> = Vec::with_capacity(members.len());
    for m in members {
        let f = m.take_frame();
        if let Some(ref frame) = f {
            m.level.store(mixer::level_to_wire(frame), Ordering::Relaxed);
        } else {
            m.level.store(0, Ordering::Relaxed);
        }
        frames.push(TickMember { session: m.clone(), frame: f, placed: placed(m, truths, now) });
    }

    let mut out = Vec::with_capacity(members.len());

    for (li, listener) in members.iter().enumerate() {
        let lstate = frames[li].placed;
        let listening = listener.joined.load(Ordering::Acquire);
        let mut contributions: Vec<mixer::Contribution<'_>> = Vec::new();
        // A listener that has not joined is mixed nothing at all (silence).
        let speakers: &[TickMember] = if listening { &frames } else { &[] };

        for tm in speakers {
            let speaker = &tm.session;
            if speaker.id == listener.id {
                continue; // never echo a listener back to themselves
            }
            // A speaker that has not joined is not heard: it is not in anyone's roster either.
            if !speaker.joined.load(Ordering::Acquire) {
                continue;
            }
            let Some(frame) = &tm.frame else { continue };

            let mut gain = listener.peer_gain(&speaker.agent_id);
            if gain <= 0.0 {
                continue;
            }

            let pan = if listener.spatial {
                let sstate = tm.placed;
                // A speaker who has not yet reported a position is not placed at
                // the origin — they are simply not mixed until they do.
                if !sstate.known || !lstate.known {
                    continue;
                }
                let d = {
                    let dx = sstate.avatar_pos[0] - lstate.listener_pos[0];
                    let dy = sstate.avatar_pos[1] - lstate.listener_pos[1];
                    let dz = sstate.avatar_pos[2] - lstate.listener_pos[2];
                    (dx * dx + dy * dy + dz * dz).sqrt()
                };
                let dg = mixer::distance_gain(d);
                if dg <= 0.0 {
                    continue; // out of range entirely
                }
                gain *= dg;
                mixer::pan_weights(sstate.avatar_pos, lstate.listener_pos, lstate.listener_rot)
            } else {
                // Group/IM voice is not positional: centre everyone.
                (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2)
            };

            contributions.push(mixer::Contribution { frame, gain, pan });
        }

        if listening && listener.spatial && lstate.known {
            for npc in npcs {
                if !npc.audible_to(&listener.agent_id) {
                    continue;
                }
                let mut gain = listener.peer_gain(&npc.speaker) * npc.gain;
                if gain <= 0.0 {
                    continue;
                }
                let d = {
                    let dx = npc.pos[0] - lstate.listener_pos[0];
                    let dy = npc.pos[1] - lstate.listener_pos[1];
                    let dz = npc.pos[2] - lstate.listener_pos[2];
                    (dx * dx + dy * dy + dz * dz).sqrt()
                };
                let dg = mixer::distance_gain(d);
                if dg <= 0.0 {
                    continue;
                }
                gain *= dg;
                let pan = mixer::pan_weights(npc.pos, lstate.listener_pos, lstate.listener_rot);
                contributions.push(mixer::Contribution { frame: &npc.frame, gain, pan });
            }
        }

        let mut stereo = vec![0.0f32; FRAME_SAMPLES * 2];
        mixer::mix_stereo(&contributions, &mut stereo);

        out.push(ListenerOutput {
            session: listener.clone(),
            stereo,
            roster: build_roster(listener, &frames),
        });
    }

    out
}

/// Build the roster update for one listener, or None when nothing changed.
///
/// The viewer turns these updates into its whole voice UI: the participant list,
/// the nametag dot and the level meter all come from `participantState`.
///
/// For any agent OTHER than itself, the only thing that creates one is a `"j"`
/// entry here — llvoicewebrtc.cpp:3845-3848 is the sole call to
/// `addParticipantByID`. With no participant, `getIsSpeaking` and
/// `getCurrentPower` return false/0 for that agent (:1786-1820), and the whole
/// nametag visualizer is switched off, because `LLVOAvatar` gates it on
/// `getVoiceEnabled` -> `isParticipant` (llvoavatar.cpp:3212-3213,
/// llvoiceclient.cpp:760-763, llvoicewebrtc.cpp:1295-1302).
///
/// Its OWN entry is the exception: Firestorm adds that locally as soon as the
/// peer connection is established (llvoicewebrtc.cpp:1020), so a Firestorm user
/// always has their own dot whatever we send. WolfStorm has no such path.
fn build_roster(listener: &Arc<Session>, frames: &[TickMember]) -> Option<String> {
    // A roster we cannot deliver must never be recorded as delivered. The data
    // channel opens roughly a second AFTER the session is registered — the region
    // has to relay our SDP answer and the viewer then has to finish ICE and DTLS
    // — and the mixer runs every 20 ms throughout. Anything computed in that
    // window is thrown away by the sender, so building it here would burn the
    // one-shot join announcement on a message nobody receives.
    if !listener.ready.load(Ordering::Acquire) {
        return None;
    }
    // An open channel is not yet a listening viewer. The Firestorm / WolfViewer data observer is
    // registered LATER than the channel opens, and anything that arrives in between is dropped
    // without a trace, while our send succeeds and the joins are recorded as told for ever:
    //   llwebrtc.cpp:1749-1754   OnStateChange(kOpen) calls OnDataChannelReady on a webrtc thread;
    //   llvoicewebrtc.cpp:3914   which only POSTS the observer registration to the main queue;
    //   llwebrtc.cpp:1770-1776   OnMessage hands data to mDataObserverList, still empty until then.
    // Our first roster leaves within one 20 ms tick of the channel opening, so it lost that race:
    // the viewer never created the participants already in the room (only a "j" does,
    // llvoicewebrtc.cpp:3845-3848), so it had no voice dot for any of them, though it heard them
    // (audio does not go through the data channel). The viewer speaks first: its join is sent
    // after the observer exists (sendJoin needs mWebRTCDataInterface, set beside the observer at
    // :3923-3924), so a roster is only sent once this listener has joined.
    if !listener.joined.load(Ordering::Acquire) {
        return None;
    }

    let mut entries: Vec<(String, RosterEntry)> = Vec::new();
    let mut announced = listener.announced.lock();

    for tm in frames {
        let speaker = &tm.session;
        // A speaker that has not announced itself yet has no meaningful primary
        // flag, and announcing it as non-primary is worse than saying nothing:
        // llvoicewebrtc.cpp:3845-3848 refuses to create a participant from a
        // spatial join with `"p":false`.
        if !speaker.joined.load(Ordering::Acquire) {
            continue;
        }

        let primary = speaker.primary.load(Ordering::Relaxed);
        let power = speaker.level.load(Ordering::Relaxed);
        let now = Announced { primary, power };

        // Report every participant including the listener: the viewer keys its own
        // participant list off these ids and shows its own dot from them too. Say
        // nothing while our belief is still accurate, so an idle room is silent on
        // the wire.
        let previous = announced.insert(speaker.agent_id.clone(), now);
        if previous == Some(now) {
            continue;
        }
        // Re-announce the join whenever the primary flag changes, which is how the
        // viewer's own second sendJoin (:3018, `"p":true`) reaches its peers.
        let joined = match previous {
            None => true,
            Some(prev) => prev.primary != primary,
        };

        entries.push((
            speaker.agent_id.clone(),
            RosterEntry {
                joined,
                primary,
                power,
                speaking: (power as f32 / proto::LEVEL_SCALE_TO_WIRE)
                    > proto::SPEAKING_AUDIO_LEVEL,
                moderator_muted: false,
            },
        ));
    }

    if entries.is_empty() {
        return None;
    }
    Some(proto::roster_json(&entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully established session: data channel open, join announced as primary.
    /// That is the steady state every test other than the lifecycle ones assumes.
    fn session(id: &str, agent: &str, pos: [f64; 3]) -> Arc<Session> {
        let s = bare_session(id, agent);
        {
            let mut st = s.state.write();
            st.avatar_pos = pos;
            st.listener_pos = pos;
            st.known = true;
        }
        s.set_ready(true);
        s.joined.store(true, Ordering::Relaxed);
        s.primary.store(true, Ordering::Relaxed);
        s
    }

    /// A session as it exists between provisioning and the data channel opening.
    fn bare_session(id: &str, agent: &str) -> Arc<Session> {
        Arc::new(Session::new(
            id.into(),
            agent.into(),
            RoomKey::Spatial { region: "region".into(), parcel: 0 },
            true,
        ))
    }

    fn roster_for<'a>(out: &'a [ListenerOutput], id: &str) -> Option<&'a String> {
        out.iter()
            .find(|o| o.session.id == id)
            .and_then(|o| o.roster.as_ref())
    }

    fn loud(s: &Arc<Session>) {
        s.push_frame(vec![0.4f32; FRAME_SAMPLES]);
    }

    #[test]
    fn listener_never_hears_themselves() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        loud(&a);
        let out = mix_room(std::slice::from_ref(&a));
        assert_eq!(out.len(), 1);
        assert!(
            out[0].stereo.iter().all(|s| *s == 0.0),
            "a lone speaker must hear silence, not their own voice"
        );
    }

    #[test]
    fn nearby_speaker_is_audible_and_distant_one_is_not() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let near = session("b", "agent-b", [5.0, 0.0, 0.0]);
        loud(&near);
        let out = mix_room(&[a.clone(), near.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().any(|s| s.abs() > 0.01),
            "a speaker 5 m away must be audible"
        );

        // Beyond MAX_DISTANCE (60 m) they must be gone completely.
        let far = session("c", "agent-c", [500.0, 0.0, 0.0]);
        loud(&far);
        let out = mix_room(&[a.clone(), far.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a speaker 500 m away must be inaudible"
        );
    }

    #[test]
    fn following_a_speaker_keeps_front_voice_centred_and_audible() {
        let listener = session("listener", "agent-listener", [0.0; 3]);
        let speaker = session("speaker", "agent-speaker", [3.0, 0.0, 0.0]);
        let mut previous_power: Option<f32> = None;
        // Translate both avatars together, like walking behind a speaker.
        // The speaker's heading changes as well; it must not aim their voice.
        for step in 0..32 {
            let x = 256000.0 + step as f64;
            let z = ((step as f64) * 0.1).sin();
            listener.apply_data(&proto::ViewerDataMessage {
                avatar_pos: Some([x, 768000.0, 25.0]),
                listener_pos: Some([x, 768000.0, 25.0]),
                listener_rot: Some([0.0, 0.0, 0.0, 1.0]),
                ..Default::default()
            });
            speaker.apply_data(&proto::ViewerDataMessage {
                avatar_pos: Some([x + 3.0, 768000.0, 25.0]),
                avatar_rot: Some([0.0, 0.0, z, (1.0-z*z).sqrt()]),
                ..Default::default()
            });
            loud(&speaker);
            let outputs = mix_room(&[listener.clone(), speaker.clone()]);
            let output = outputs.iter().find(|o| o.session.id == "listener").unwrap();
            let left: f32 = output.stereo.iter().step_by(2).map(|s| s*s).sum();
            let right: f32 = output.stereo.iter().skip(1).step_by(2).map(|s| s*s).sum();
            assert!(left > 0.0 && right > 0.0, "front speech reaches both ears");
            assert!((left-right).abs() < 1e-4, "front speech stays centred");
            let power = left+right;
            if let Some(previous) = previous_power {
                assert!((previous-power).abs() < 1e-4, "common motion must not attenuate");
            }
            previous_power = Some(power);
        }
    }

    #[test]
    fn each_listener_gets_its_own_mix() {
        // The whole point of per-listener mixing: A hears B on one side while B
        // hears A on the other. A shared room mix cannot express this.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [0.0, -5.0, 0.0]);
        loud(&a);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);

        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        let for_b = out.iter().find(|o| o.session.id == "b").unwrap();

        // Both face local +X: B is south (-Y), on A's right.
        let a_left: f32 = for_a.stereo.iter().step_by(2).map(|s| s.abs()).sum();
        let a_right: f32 = for_a.stereo.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(a_right > a_left, "A should hear B to the right");

        // A is north (+Y), on B's left.
        let b_left: f32 = for_b.stereo.iter().step_by(2).map(|s| s.abs()).sum();
        let b_right: f32 = for_b.stereo.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(b_left > b_right, "B should hear A to the left");
    }

    #[test]
    fn per_listener_mute_and_gain_are_respected() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        a.mutes.write().insert("agent-b".into(), true);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a muted speaker must be silent for that listener only"
        );

        // Half gain must be quieter than unity, and it must not affect anyone else.
        let c = session("c", "agent-c", [0.0, 0.0, 0.0]);
        c.gains.write().insert("agent-b".into(), 0.5);
        let d = session("d", "agent-d", [0.0, 0.0, 0.0]);
        loud(&b);
        let out = mix_room(&[c.clone(), d.clone(), b.clone()]);
        let for_c: f32 = out.iter().find(|o| o.session.id == "c").unwrap()
            .stereo.iter().map(|s| s.abs()).sum();
        let for_d: f32 = out.iter().find(|o| o.session.id == "d").unwrap()
            .stereo.iter().map(|s| s.abs()).sum();
        assert!(for_d > for_c * 1.5, "half gain should be clearly quieter: c={for_c} d={for_d}");
    }

    #[test]
    fn position_unknown_speakers_are_not_placed_at_the_origin() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let ghost = bare_session("g", "agent-g");
        ghost.push_frame(vec![0.9f32; FRAME_SAMPLES]);
        let out = mix_room(&[a.clone(), ghost.clone()]);
        let for_a = out.iter().find(|o| o.session.id == "a").unwrap();
        assert!(
            for_a.stereo.iter().all(|s| s.abs() < 1e-6),
            "a speaker with no reported position must not be audible at <0,0,0>"
        );
    }

    #[test]
    fn roster_announces_a_join_once() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("first tick announces the join").clone();
        assert!(r.contains("agent-b"), "first tick announces the join: {r}");
        assert!(r.contains(r#""j":{"p":true}"#), "spatial joins must be primary: {r}");

        // Second tick, speaking at a different level: a power update, but no
        // repeated join for the same agent.
        b.push_frame(vec![0.2f32; FRAME_SAMPLES]);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("a changed level is reported").clone();
        assert!(!r.contains(r#""j""#), "join must not repeat: {r}");
        assert!(r.contains("agent-b"), "the level update still names the speaker: {r}");
    }

    #[test]
    fn a_lone_participant_is_still_announced_to_themselves() {
        // WolfStorm builds its participant list purely from the keys we send, so a
        // user who is briefly the only person in their region must still be told
        // about themselves or they have no entry and no level meter at all.
        // (Firestorm is not affected: it adds its own participant locally at
        // llvoicewebrtc.cpp:1020. Other agents always need the "j".)
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        a.push_frame(vec![0.4f32; FRAME_SAMPLES]);

        let out = mix_room(std::slice::from_ref(&a));
        assert_eq!(out.len(), 1);
        let r = roster_for(&out, "a").expect("a lone participant still gets a roster").clone();
        assert!(r.contains("agent-a"), "and it names them: {r}");
        assert!(r.contains(r#""j":{"p":true}"#), "as a primary join: {r}");
        assert!(
            r.contains(r#""v":true"#),
            "their own speech level must be reported so their meter moves: {r}"
        );
        assert!(
            out[0].stereo.iter().all(|s| *s == 0.0),
            "but they still hear nothing but silence"
        );
    }

    #[test]
    fn no_roster_is_built_before_the_data_channel_opens() {
        // The mixer runs ~50 times a second between a session being registered and
        // its data channel opening. Nothing built in that window can be delivered,
        // so nothing may be recorded as delivered either.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.set_ready(false);
        loud(&a);
        loud(&b);

        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(
            roster_for(&out, "b").is_none(),
            "a listener with no data channel must not be handed a roster"
        );

        // Once the channel opens, the joins it missed are still owed to it.
        b.set_ready(true);
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "b").expect("the join must survive the wait").clone();
        assert!(r.contains("agent-a"), "b must finally be told about a: {r}");
        assert!(r.contains(r#""j":{"p":true}"#), "and told as a join: {r}");
    }

    #[test]
    fn a_speaker_is_not_announced_until_it_has_joined() {
        // llvoicewebrtc.cpp:3845-3848 refuses to create a participant from a
        // spatial join carrying "p":false, and there is no second chance — so a
        // speaker whose primary flag is not yet known must not be announced at all.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.joined.store(false, Ordering::Relaxed);
        b.primary.store(false, Ordering::Relaxed);
        loud(&b);

        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").cloned().unwrap_or_default();
        assert!(
            !r.contains("agent-b"),
            "a speaker that has not announced itself must not appear: {r}"
        );

        // Its own join arrives, primary this time.
        b.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("the join must be announced now").clone();
        assert!(r.contains(r#""j":{"p":true}"#), "announced as primary: {r}");
    }

    #[test]
    fn join_is_resent_when_the_primary_flag_flips() {
        // A spatial connection is built non-primary (llvoicewebrtc.cpp:3322) and
        // sends its first join that way (:2985); only at VOICE_STATE_SESSION_UP
        // does it re-send with "p":true (:3010-3017). The viewer discards the
        // first, so the second has to reach its peers as a fresh join.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        b.apply_data(&proto::ViewerDataMessage { join: Some(false), ..Default::default() });
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("the non-primary join is reported").clone();
        assert!(r.contains(r#""j":{"p":false}"#), "first join is non-primary: {r}");

        b.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("the flip must be reported").clone();
        assert!(
            r.contains(r#""j":{"p":true}"#),
            "the primary flip must re-announce the join: {r}"
        );
    }

    #[test]
    fn a_speaker_falling_silent_is_reported_once() {
        // The viewer keeps mLevel/mIsSpeaking at whatever we last sent
        // (llvoicewebrtc.cpp:3222-3238), so silence has to be sent explicitly or
        // the indicator stays lit for ever.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        loud(&b);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("join + level").clone();
        assert!(r.contains(r#""v":true"#), "a loud speaker reads as speaking: {r}");

        // b stops talking: exactly one update saying so, then nothing.
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("the fall to silence is reported").clone();
        assert!(r.contains(r#""p":0"#), "silence is reported as zero power: {r}");
        assert!(r.contains(r#""v":false"#), "and as not speaking: {r}");

        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(
            roster_for(&out, "a").is_none(),
            "a room that stays silent must not keep talking on the wire"
        );
    }

    #[test]
    fn no_roster_reaches_a_listener_before_its_own_join() {
        // The live bug: an open data channel is not yet a listening Firestorm/WolfViewer
        // (llwebrtc.cpp:1770-1776 drops what arrives before the main-queue task at
        // llvoicewebrtc.cpp:3914-3925 registers the observer). A roster sent then was lost while
        // being recorded as told, so the newcomer never got a "j" for anyone already there.
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = bare_session("b", "agent-b");
        {
            let mut st = b.state.write();
            st.listener_pos = [1.0, 0.0, 0.0];
            st.avatar_pos = [1.0, 0.0, 0.0];
            st.known = true;
        }
        b.set_ready(true); // channel open, viewer not listening yet
        loud(&a);
        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(roster_for(&out, "b").is_none(), "nothing may be sent before b's join");

        // The viewer's first join is the non-primary one (llvoicewebrtc.cpp:3580, :3597).
        b.apply_data(&proto::ViewerDataMessage { join: Some(false), ..Default::default() });
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "b").expect("b's join opens its roster").clone();
        assert!(r.contains(r#""agent-a":{"j":{"p":true}"#), "b is told a joined, as primary: {r}");
    }

    #[test]
    fn a_moved_session_gets_the_new_room_only_after_it_joins_again() {
        // After a wolf_move the viewer files data under the OLD channel until our reply re-keys it
        // (llvoicewebrtc.cpp:4046-4053); joins sent in that gap went into the session it then
        // shuts down. It joins again once re-keyed (:3213-3223), and that is when they must come.
        let reg = Registry::default();
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let there = RoomKey::Spatial { region: "other-region".into(), parcel: 0 };
        let c = Arc::new(Session::new("c".into(), "agent-c".into(), there.clone(), true));
        {
            let mut st = c.state.write();
            st.known = true;
        }
        c.set_ready(true);
        c.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        reg.insert(a.clone());
        reg.insert(c.clone());
        let _ = mix_room(&reg.members(&a.room()));

        assert!(reg.relocate("a", there.clone()).is_some());
        let out = mix_room(&reg.members(&there));
        assert!(roster_for(&out, "a").is_none(), "the mover hears no roster until it re-joins");
        let r = roster_for(&out, "c").cloned().unwrap_or_default();
        assert!(!r.contains("agent-a"), "nor is it announced before re-joining: {r}");

        a.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        let out = mix_room(&reg.members(&there));
        let r = roster_for(&out, "a").expect("the re-join opens the roster").clone();
        assert!(r.contains(r#""agent-c":{"j":{"p":true}"#), "a is told c joined: {r}");
        let r = roster_for(&out, "c").expect("c is told about a").clone();
        assert!(r.contains(r#""agent-a":{"j":{"p":true}"#), "c is told a joined: {r}");
    }

    #[test]
    fn a_closed_data_channel_forgets_what_the_listener_was_told() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        let _ = mix_room(&[a.clone(), b.clone()]);

        // The channel drops and comes back; the viewer's participant list went
        // with it, so every join is owed again.
        a.set_ready(false);
        a.set_ready(true);
        let out = mix_room(&[a.clone(), b.clone()]);
        let r = roster_for(&out, "a").expect("joins must be re-announced").clone();
        assert!(r.contains(r#""j""#), "a reopened channel gets fresh joins: {r}");
    }

    #[test]
    fn silent_room_produces_no_roster_traffic() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        // First tick announces joins.
        let _ = mix_room(&[a.clone(), b.clone()]);
        // Second tick, nobody speaking, nothing new to say.
        let out = mix_room(&[a.clone(), b.clone()]);
        assert!(
            out.iter().all(|o| o.roster.is_none()),
            "an idle room must not push a roster every tick"
        );
    }

    #[test]
    fn removing_a_session_drops_its_room_entry() {
        let reg = Registry::default();
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let room = a.room();
        reg.insert(a);
        assert_eq!(reg.members(&room).len(), 1);
        reg.remove("a");
        assert_eq!(reg.session_count(), 0);
        assert!(reg.rooms().is_empty(), "empty rooms must not linger");
    }

    /// [2026-10-22] The first recorded end cause is the one logged; none recorded reads "unknown".
    #[test]
    fn end_reason_keeps_the_first_cause() {
        let s = bare_session("s1", "a1");
        assert_eq!(s.end_reason(), "unknown");
        s.set_end_reason("viewer logout");
        s.set_end_reason("connection closed");
        assert_eq!(s.end_reason(), "viewer logout");
    }

    /// [2026-10-22] An open data channel with no join is reported once, and never once the viewer has joined.
    #[test]
    fn join_overdue_fires_once_and_not_after_a_join() {
        let s = bare_session("s1", "a1");
        assert!(!s.join_overdue(std::time::Duration::ZERO), "not before the data channel is open");
        s.set_ready(true);
        assert!(s.join_overdue(std::time::Duration::ZERO));
        assert!(!s.join_overdue(std::time::Duration::ZERO), "only once");
        let j = bare_session("s2", "a2");
        j.set_ready(true);
        j.joined.store(true, Ordering::Release);
        assert!(!j.join_overdue(std::time::Duration::ZERO));
    }

    #[test]
    fn jitter_buffer_drops_oldest_rather_than_growing() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        for i in 0..(JITTER_MAX_FRAMES + 5) {
            a.push_frame(vec![i as f32; FRAME_SAMPLES]);
        }
        assert_eq!(a.inbound.lock().len(), JITTER_MAX_FRAMES);
        // The surviving frames must be the NEWEST ones.
        let first = a.take_frame().unwrap()[0];
        assert_eq!(first, 5.0);
    }

    #[test]
    fn gain_conversion_uses_the_viewer_factor() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        // 220 == unity per PEER_GAIN_CONVERSION_FACTOR.
        let m = proto::ViewerDataMessage {
            user_gain: vec![("agent-b".into(), 220)],
            ..Default::default()
        };
        a.apply_data(&m);
        assert!((a.peer_gain("agent-b") - 1.0).abs() < 1e-6);

        let m = proto::ViewerDataMessage {
            user_gain: vec![("agent-b".into(), 110)],
            ..Default::default()
        };
        a.apply_data(&m);
        assert!((a.peer_gain("agent-b") - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tether_is_reapplied_server_side() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        // A hostile client claims its ear is 1 km away from its avatar.
        let m = proto::ViewerDataMessage {
            avatar_pos: Some([0.0, 0.0, 0.0]),
            listener_pos: Some([1000.0, 0.0, 0.0]),
            ..Default::default()
        };
        a.apply_data(&m);
        let st = *a.state.read();
        let d = (st.listener_pos[0] - st.avatar_pos[0]).abs();
        assert!(
            (d - mixer::MAX_AUDIO_DIST).abs() < 1e-6,
            "ear must be clamped to 50 m, got {d}"
        );
    }

    fn npc_frame(pos: [f64; 3], listeners: Option<Vec<&str>>) -> Arc<crate::npc::NpcFrame> {
        Arc::new(crate::npc::NpcFrame {
            scene: "region".into(),
            speaker: "npc-1".into(),
            pos,
            gain: 1.0,
            listeners: listeners.map(|l| Arc::new(l.into_iter().map(String::from).collect())),
            frame: vec![0.4f32; FRAME_SAMPLES],
        })
    }

    #[test]
    fn a_story_character_is_heard_by_distance_and_listener_list() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [200.0, 0.0, 0.0]);
        let near = npc_frame([5.0, 0.0, 0.0], None);
        let out = mix_room_with(&[a.clone(), b.clone()], &[near], &Truths::default());
        let heard = |id: &str| out.iter().find(|o| o.session.id == id).unwrap().stereo.iter().any(|s| s.abs() > 0.01);
        assert!(heard("a"), "5 m away is audible");
        assert!(!heard("b"), "195 m away is not");
        let r = roster_for(&out, "a").cloned().unwrap_or_default();
        assert!(!r.contains("npc-1"), "a story character is never put in the roster: {r}");

        // Limited to listed agents (for example a class).
        let only_b = npc_frame([5.0, 0.0, 0.0], Some(vec!["agent-b"]));
        let out = mix_room_with(std::slice::from_ref(&a), &[only_b], &Truths::default());
        assert!(out[0].stereo.iter().all(|s| s.abs() < 1e-6), "a listener not on the list hears nothing");

        // The listener's own mute applies to the character's uuid.
        a.mutes.write().insert("npc-1".into(), true);
        let out = mix_room_with(std::slice::from_ref(&a), &[npc_frame([1.0, 0.0, 0.0], None)], &Truths::default());
        assert!(out[0].stereo.iter().all(|s| s.abs() < 1e-6), "muting the character silences it");
    }

    #[test]
    fn a_session_moves_rooms_without_being_recreated() {
        let reg = Registry::default();
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let b = session("b", "agent-b", [1.0, 0.0, 0.0]);
        let here = a.room();
        let there = RoomKey::Spatial { region: "other-region".into(), parcel: 0 };
        let c = Arc::new(Session::new("c".into(), "agent-c".into(), there.clone(), true));
        reg.insert(a.clone());
        reg.insert(b.clone());
        reg.insert(c);

        // b has been told about a.
        let _ = mix_room(&reg.members(&here));

        assert_eq!(reg.relocate("a", there.clone()), Some(here.clone()));
        assert_eq!(a.room(), there, "the session itself carries its new room");
        assert_eq!(reg.members(&here).len(), 1, "only b is left behind");
        assert_eq!(reg.members(&there).len(), 2, "a has joined c");
        assert!(Arc::ptr_eq(&reg.get("a").unwrap(), &a), "the same session object, not a new one");
        assert!(!a.joined.load(Ordering::Acquire), "a real move gates the roster until the re-join");
        a.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        assert_eq!(reg.relocate("a", there.clone()), None, "moving to where it already is changes nothing");
        assert!(a.joined.load(Ordering::Acquire), "a no-op move must not gate the session's roster");
        assert_eq!(reg.relocate("nobody", here), None);
    }

    fn audible(out: &[ListenerOutput], id: &str) -> bool {
        out.iter().find(|o| o.session.id == id).unwrap().stereo.iter().any(|s| s.abs() > 0.01)
    }

    /// [HARDENING 2026-10-23] Gap 1: a session whose data channel is open but whose viewer never
    /// joined hears nothing, and is heard by nobody, until it joins.
    #[test]
    fn a_session_that_never_joined_neither_hears_nor_is_heard() {
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let hidden = session("h", "agent-h", [1.0, 0.0, 0.0]);
        hidden.joined.store(false, Ordering::Release); // channel open, no "j"
        loud(&a);
        loud(&hidden);
        let out = mix_room(&[a.clone(), hidden.clone()]);
        assert!(!audible(&out, "h"), "a hidden listener must not get the room's audio");
        assert!(!audible(&out, "a"), "a speaker that never joined must not be heard");

        // Its join opens both directions, as for stock Firestorm (sendJoin on data channel ready).
        hidden.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        loud(&a);
        loud(&hidden);
        let out = mix_room(&[a.clone(), hidden.clone()]);
        assert!(audible(&out, "h") && audible(&out, "a"), "after the join both hear each other");

        // The same for a non-spatial (call / group) room.
        let room = RoomKey::MultiAgent { channel: "group-g".into() };
        let c = Arc::new(Session::new("c".into(), "agent-c".into(), room.clone(), false));
        let d = Arc::new(Session::new("d".into(), "agent-d".into(), room, false));
        c.set_ready(true);
        d.set_ready(true);
        c.apply_data(&proto::ViewerDataMessage { join: Some(true), ..Default::default() });
        loud(&c);
        loud(&d);
        let out = mix_room(&[c.clone(), d.clone()]);
        assert!(!audible(&out, "d"), "an unjoined call participant hears nothing");
        assert!(!audible(&out, "c"), "and is not heard");
    }

    /// [HARDENING 2026-10-23] Gap 7: with a fresh region position, a viewer that claims to stand
    /// somewhere else is mixed from (near) where its region says it is, and its ear stays within
    /// 50 m of that true position.
    #[test]
    fn a_false_position_is_pulled_back_to_the_regions_position() {
        let truths = Truths::default();
        let now = std::time::Instant::now();
        // The speaker really is 500 m away (region feed; z - 1 m because the head bump is added).
        let a = session("a", "agent-a", [0.0, 0.0, 0.0]);
        let liar = session("l", "agent-l", [2.0, 0.0, 0.0]); // claims to stand beside a
        truths.report("agent-a", "r", [0.0, 0.0, -1.0], [0.0; 3], now);
        truths.report("agent-l", "r", [500.0, 0.0, -1.0], [0.0; 3], now);
        loud(&a);
        let out = mix_room_with(&[a.clone(), liar.clone()], &[], &truths);
        assert!(!audible(&out, "l"), "the liar must not hear a from where it claims to be");
        loud(&liar);
        let out = mix_room_with(&[a.clone(), liar.clone()], &[], &truths);
        assert!(!audible(&out, "a"), "nor be heard there");

        // Its ear 50 m from the TRUE position at most: an ear claimed beside a is tethered back.
        {
            let mut st = liar.state.write();
            st.avatar_pos = [470.0, 0.0, 0.0];
            st.listener_pos = [1.0, 0.0, 0.0];
        }
        loud(&a);
        let out = mix_room_with(&[a.clone(), liar.clone()], &[], &truths);
        assert!(!audible(&out, "l"), "an ear claimed near a is tethered to 50 m of the true position");

        // An honest viewer a few metres off its region's position is mixed as it says.
        let b = session("b", "agent-b", [5.0, 0.0, 0.0]);
        truths.report("agent-b", "r", [3.0, 0.0, -1.0], [0.0; 3], now);
        loud(&a);
        let out = mix_room_with(&[a.clone(), b.clone()], &[], &truths);
        assert!(audible(&out, "b"), "an honest nearby listener still hears");

        // No region position (a region still on the old module): the viewer's claim is used, as before.
        let c = session("c", "agent-c", [2.0, 0.0, 0.0]);
        loud(&a);
        let out = mix_room_with(&[a.clone(), c.clone()], &[], &truths);
        assert!(audible(&out, "c"), "without a region position nothing changes");
    }
}
