//
// Copyright 2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::{
    cmp::{max, min},
    collections::{HashMap, HashSet},
    convert::{From, TryFrom},
    fmt::{self, Display, Formatter},
    sync::Arc,
    time::SystemTime,
};

use calling_common::{
    ClientStatus, DataRate, DataRateTracker, DemuxId, Duration, Instant, PixelSize, RoomId,
    VideoHeight,
};
use hex::ToHex;
use log::*;
use mrp::{self, MrpReceiveError, MrpStream};
use prost::Message;
use reqwest::Url;
use serde::Serialize;
use strum_macros::{EnumIter, EnumString};
use thiserror::Error;

use crate::{
    audio,
    connection::ConnectionRates,
    protos,
    region::RegionRelation,
    rtp::{self, VideoRotation},
    vp8,
};

mod approval_persistence;
use approval_persistence::ApprovedUsers;

pub const CLIENT_SERVER_DATA_SSRC: rtp::Ssrc = 1;
pub const CLIENT_SERVER_DATA_PAYLOAD_TYPE: rtp::PayloadType = 101;

/// This is for throttling the CPU usage of calculating what key frame requests
/// to send.  A higher value should use less CPU and a lower value should
/// send key frame requests with less delay.
const KEY_FRAME_REQUEST_CALCULATION_INTERVAL: Duration = Duration::from_millis(5);
/// For a particular SSRC, we only want to send a key frame request this often.
/// Sending more often than this probably doesn't help any and wastes bandwidth.
const KEY_FRAME_REQUEST_RESEND_INTERVAL: Duration = Duration::from_millis(200);
/// Don't send a key frame request more frequently than this for high resolution
/// video. Key frames for higher resolution video tend to use more bandwidth.
const HIGH_RES_KEY_FRAME_REQUEST_RESEND_INTERVAL: Duration = Duration::from_millis(500);
/// Even if the target send rate changes really frequently,
/// don't reallocate it more often than this.
/// A lower value uses more CPU but makes layer switching more reactive.
const SEND_RATE_REALLOCATION_INTERVAL: Duration = Duration::from_millis(1000);
/// This is how often we recaculate the active speaker.
/// The lower the value, the more CPU we use but the more responsive
/// active speaker switching becomes.
const ACTIVE_SPEAKER_CALCULATION_INTERVAL: Duration = Duration::from_millis(300);
/// This is how often we send stats down to the client
const STATS_MESSAGE_INTERVAL: Duration = Duration::from_secs(1);
/// This is how often we send update messages to removed clients.
const REMOVED_CLIENTS_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
/// This is how often we send raised hands messages to clients.
const RAISED_HANDS_MESSAGE_INTERVAL: Duration = Duration::from_secs(1);
/// This should match the buffer size used by clients
const MAX_MRP_WINDOW_SIZE: usize = 64;
/// How long the SFU waits for an MRP ack before resending
const MRP_SEND_TIMEOUT_INTERVAL: Duration = Duration::from_secs(1);
/// How long the generations are for minimum target send rate; layer
/// allocation uses the minimum target rate over the past two generations.
const MIN_TARGET_SEND_RATE_GENERATION_INTERVAL: Duration = Duration::from_millis(2500);
/// How much of the target send rate to allocate when the queue drain rate is high.
const TARGET_RATE_MINIMUM_ALLOCATION_RATIO: f64 = 0.9;

#[derive(Debug, EnumString, EnumIter, Clone, Copy, Eq, PartialEq, Hash)]
pub enum CallSizeBucket {
    Empty,
    Solo,
    Pair,
    From3To6,
    From7To9,
    From10To19,
    From20To29,
    From30To49,
    From50To79,
    BeyondLimit,
}

impl CallSizeBucket {
    pub const fn as_tag(&self) -> &'static str {
        match self {
            Self::Empty => "call-size:0",
            Self::Solo => "call-size:1",
            Self::Pair => "call-size:2",
            Self::From3To6 => "call-size:3-6",
            Self::From7To9 => "call-size:7-9",
            Self::From10To19 => "call-size:10-19",
            Self::From20To29 => "call-size:20-29",
            Self::From30To49 => "call-size:30-49",
            Self::From50To79 => "call-size:50-79",
            Self::BeyondLimit => "call-size:BEYOND_LIMIT",
        }
    }
}

impl From<usize> for CallSizeBucket {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::Empty,
            1 => Self::Solo,
            2 => Self::Pair,
            i if (3..=6).contains(&i) => Self::From3To6,
            i if (7..=9).contains(&i) => Self::From7To9,
            i if (10..=19).contains(&i) => Self::From10To19,
            i if (20..=29).contains(&i) => Self::From20To29,
            i if (30..=49).contains(&i) => Self::From30To49,
            i if (50..=79).contains(&i) => Self::From50To79,
            _ => Self::BeyondLimit,
        }
    }
}

/// A wrapper around Vec<u8> to identify a Call.
/// It comes from signaling, but isn't known by the clients.
// It would be easier to change this to a u64, and we don't have
// to change the clients to do so.  Just the SFU frontend.
// Note that this is deliberately not Debug; see LoggableCallId.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct CallId(Arc<[u8]>);

impl Serialize for CallId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_slice().encode_hex::<String>().as_str())
    }
}

impl From<Vec<u8>> for CallId {
    fn from(call_id: Vec<u8>) -> Self {
        Self(call_id.into())
    }
}

impl CallId {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

/// A truncated call ID that is suitable for logging.
#[derive(Clone, Debug)]
pub struct LoggableCallId {
    /// The truncated hex string version of this call's id.
    truncated_call_id_for_logging: String,
}

impl From<&CallId> for LoggableCallId {
    fn from(call_id: &CallId) -> Self {
        Self::from(call_id.as_slice())
    }
}

impl From<&[u8]> for LoggableCallId {
    fn from(data: &[u8]) -> Self {
        let truncated_call_id_for_logging = {
            if data.is_empty() {
                "<EMPTY>".to_string()
            } else {
                let first_3_bytes_of_id = data.chunks(3).next().unwrap();
                first_3_bytes_of_id.encode_hex::<String>()
            }
        };
        LoggableCallId {
            truncated_call_id_for_logging,
        }
    }
}

impl Display for LoggableCallId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.truncated_call_id_for_logging)
    }
}

/// A wrapper around String to identify a user.
///
/// It comes from signaling and is actually an opaque value generated by clients, not a UUID.
///
/// UserId deliberately does not implement Display or Debug; it will be consistent across calls in
/// the same group and is thus considered sensitive.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct UserId(String);

impl From<String> for UserId {
    fn from(user_id: String) -> Self {
        Self(user_id)
    }
}

impl From<UserId> for String {
    fn from(value: UserId) -> Self {
        value.0
    }
}

impl UserId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

serde_with::serde_conv!(
    pub UserIdAsStr,
    UserId,
    UserId::as_str,
    |value: String| -> Result<_, std::convert::Infallible> { Ok(value.into()) }
);

trait DemuxIdExt {
    fn from_ssrc(ssrc: rtp::Ssrc) -> Self;
}
impl DemuxIdExt for DemuxId {
    fn from_ssrc(ssrc: rtp::Ssrc) -> Self {
        Self::try_from(ssrc & 0b1111_1111_1111_1111_1111_1111_1111_0000)
            .expect("valid with low bits masked")
    }
}

/// Identifies one of the "layers" that can be combined with a
/// DemuxID to create an SSRC.  Can be inferred from an SSRC.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum LayerId {
    // SSRC offsets 1, 3, 5, and 7 are for RTX.
    Audio = 0,
    Video0 = 2,
    Video1 = 4,
    Video2 = 6,
    RtpData = 0xD,
}

impl LayerId {
    fn from_ssrc(ssrc: rtp::Ssrc) -> Option<Self> {
        Some(match (ssrc & 0b1111) as u8 {
            0 => LayerId::Audio,
            2 => LayerId::Video0,
            4 => LayerId::Video1,
            6 => LayerId::Video2,
            0xD => LayerId::RtpData,
            _ => {
                return None;
            }
        })
    }

    fn from_video_layer_index(video_layer_index: usize) -> Option<Self> {
        Some(match video_layer_index {
            0 => LayerId::Video0,
            1 => LayerId::Video1,
            2 => LayerId::Video2,
            _ => {
                return None;
            }
        })
    }

    fn layer_index_from_ssrc(ssrc: rtp::Ssrc) -> Option<usize> {
        match Self::from_ssrc(ssrc) {
            Some(LayerId::Video0) => Some(0),
            Some(LayerId::Video1) => Some(1),
            Some(LayerId::Video2) => Some(2),
            _ => None,
        }
    }

    pub fn to_ssrc(self, demux_id: DemuxId) -> rtp::Ssrc {
        u32::from(demux_id) | (self as u32)
    }

    pub fn to_rtx_ssrc(self, demux_id: DemuxId) -> rtp::Ssrc {
        rtp::to_rtx_ssrc(self.to_ssrc(demux_id))
    }
}

#[derive(Error, Debug, Eq, PartialEq)]
pub enum Error {
    #[error("received RTP data for server with invalid protobuf")]
    InvalidClientToServerProtobuf,
    #[error("received RTP packet with unauthorized SSRC.  Authorized DemuxId: {0:?}.  Received DemuxId: {1:?}")]
    UnauthorizedRtpSsrc(DemuxId, DemuxId),
    #[error("received RTP packet with invalid VP8 header")]
    InvalidVp8Header,
    #[error("received RTP packet with invalid MRP header")]
    InvalidMrpHeader(MrpReceiveError),
    #[error("received RTP packet with invalid layer ID")]
    InvalidRtpLayerId,
    #[error("unknown demux ID: {0:?}")]
    UnknownDemuxId(DemuxId),
    #[error("received RTP leave")]
    Leave,
}

/// Represents an RTP packet that should be sent to a particular client
/// of the call, identified by DemuxId.
type RtpToSend = (DemuxId, rtp::Packet<Vec<u8>>);
/// Represents a KeyFrameRequest that should be sent to a particular client
/// of the call, identified by DemuxId.
type KeyFrameRequestToSend = (DemuxId, rtp::KeyFrameRequest);

pub enum CallActivity {
    Active,
    Inactive,
    Waiting,
}

/// A collection of clients between which media is forwarded.
/// Each client sends and receives media (audio, video, or data).
/// Media is forwarded from every client to every other client.
/// Video is constrained by congestion control and video requests.
/// Request for video key frames are also forwarded.
/// Key frame requests may be generated when to allow for switching between
/// different video spatial layers.
pub struct Call {
    // Immutable
    room_id: Option<RoomId>,
    loggable_call_id: LoggableCallId,
    creator_id: UserId, // AKA the first user to join
    new_clients_require_approval: bool,
    persist_approval_for_all_users_who_join: bool,
    created: SystemTime, // For knowing how old the call is
    active_speaker_message_interval: Duration,
    initial_target_send_rate: DataRate,
    default_requested_max_send_rate: DataRate,

    /// Clients (AKA devices) that have joined the call
    clients: Vec<Client>,
    /// Clients that have yet to be approved by an admin
    pending_clients: Vec<NonParticipantClient>,
    /// Clients that have been removed by an admin but haven't yet disconnected
    removed_clients: Vec<NonParticipantClient>,
    /// The last time a client was added or removed, including pending clients
    client_added_or_removed: Instant,
    /// The last time a clients update was sent to the clients
    clients_update_sent: Instant,

    /// Clients that are considered pre-approved to join the call
    approved_users: ApprovedUsers,
    /// Clients that have denied approval to join the call.
    ///
    /// Repeated denial is implicitly promoted to a block; approval clears any remembered denial.
    denied_users: HashSet<UserId>,
    /// Clients that have been blocked from joining the call
    ///
    /// Takes precedent over `approved_users`.
    blocked_users: HashSet<UserId>,

    /// The active speaker, if there is one
    /// This is calculated based on incoming audio levels
    active_speaker_id: Option<DemuxId>,
    /// The last time the active speaker was calculated
    active_speaker_calculated: Instant,
    /// The last time an active speaker update was sent to the clients
    active_speaker_update_sent: Instant,

    /// A list of clients with the status of their raised hand
    raised_hands: Option<Vec<RaisedHand>>,
    /// The latest sequence number of each client in the raised_hands list
    raised_hands_seqnums: HashMap<DemuxId, u32>,

    /// The last time a status update was sent to the clients
    stats_update_sent: Instant,
    /// The last time an update was sent to removed clients
    removed_clients_update_sent: Instant,
    /// The last time a raised hands update was sent to clients
    raised_hands_sent: Instant,

    /// The last time key frame requests were sent, in general and specifically for certain SSRCs
    key_frame_requests_sent: Instant,
    key_frame_request_sent_by_ssrc: HashMap<rtp::Ssrc, Instant>,
    call_time: CallTimeStats,
}

#[derive(Default)]
pub struct CallTimeStats {
    pub empty: Duration,
    pub solo: Duration,
    pub pair: Duration,
    pub many: Duration,
}

/// Info we need to transfer from the Call to the Connection
/// In particular, we need to be able to do 2 things:
/// 1.  Send padding at a certain rate.
/// 2.  Reset congestion control
#[derive(Debug, PartialEq, Eq)]
pub struct SendRateAllocationInfo {
    pub demux_id: DemuxId,
    pub padding_ssrc: Option<rtp::Ssrc>,
    pub target_send_rate: DataRate,
    pub requested_base_rate: DataRate,
    pub ideal_send_rate: DataRate,
}

pub struct RaisedHand {
    pub demux_id: DemuxId,
    pub raise: bool,
}

impl Call {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        loggable_call_id: LoggableCallId,
        room_id: Option<RoomId>,
        creator_id: UserId,
        new_clients_require_approval: bool,
        persist_approval_for_all_users_who_join: bool,
        active_speaker_message_interval: Duration,
        initial_target_send_rate: DataRate,
        default_requested_max_send_rate: DataRate,
        now: Instant,
        system_now: SystemTime,
        approved_users: Option<Vec<UserId>>,
        approved_users_persistence_url: Option<&'static Url>,
    ) -> Self {
        info!("call: {} creating", loggable_call_id);
        Self {
            room_id: room_id.clone(),
            loggable_call_id,
            creator_id,
            new_clients_require_approval,
            persist_approval_for_all_users_who_join,
            created: system_now,
            active_speaker_message_interval,
            initial_target_send_rate,
            default_requested_max_send_rate,

            clients: Vec::new(),
            pending_clients: Vec::new(),
            removed_clients: Vec::new(),
            client_added_or_removed: now,
            clients_update_sent: now,

            approved_users: ApprovedUsers::new(
                approved_users.unwrap_or_default(),
                approved_users_persistence_url.zip(room_id),
            ),
            denied_users: HashSet::new(),
            blocked_users: HashSet::new(),

            active_speaker_id: None,
            active_speaker_calculated: now - ACTIVE_SPEAKER_CALCULATION_INTERVAL, // easier than using None :)
            active_speaker_update_sent: now,

            raised_hands: None,
            raised_hands_seqnums: HashMap::new(),

            stats_update_sent: now, // easier than using None :)
            removed_clients_update_sent: now - REMOVED_CLIENTS_UPDATE_INTERVAL, // easier than using None :)
            raised_hands_sent: now - RAISED_HANDS_MESSAGE_INTERVAL,

            key_frame_requests_sent: now - KEY_FRAME_REQUEST_CALCULATION_INTERVAL, // easier than using None :)
            key_frame_request_sent_by_ssrc: HashMap::new(),
            call_time: CallTimeStats::default(),
        }
    }

    pub fn room_id(&self) -> Option<&RoomId> {
        self.room_id.as_ref()
    }

    pub fn loggable_call_id(&self) -> &LoggableCallId {
        &self.loggable_call_id
    }

    pub fn creator_id(&self) -> &UserId {
        &self.creator_id
    }

    pub fn size(&self) -> usize {
        self.clients.len()
    }

    pub fn size_bucket(&self) -> CallSizeBucket {
        CallSizeBucket::from(self.size())
    }

    pub fn activity(&mut self, now: &Instant, inactivity_timeout: &Duration) -> CallActivity {
        if !self.clients.is_empty()
            || !self.pending_clients.is_empty()
            || !self.removed_clients.is_empty()
        {
            return CallActivity::Active;
        }

        self.approved_users.tick();
        if !self.approved_users.is_busy()
            && *now >= self.client_added_or_removed + *inactivity_timeout
        {
            CallActivity::Inactive
        } else {
            CallActivity::Waiting
        }
    }

    pub fn is_approved_users_busy(&self) -> bool {
        self.approved_users.is_busy()
    }

    pub fn created(&self) -> SystemTime {
        self.created
    }

    pub fn call_time(&self) -> &CallTimeStats {
        &self.call_time
    }

    pub fn has_client(&self, demux_id: DemuxId) -> bool {
        self.clients
            .iter()
            .any(|client| client.demux_id == demux_id)
            || self
                .pending_clients
                .iter()
                .any(|client| client.demux_id == demux_id)
            || self
                .removed_clients
                .iter()
                .any(|client| client.demux_id == demux_id)
    }

    pub fn is_admin(&self, user_id: &UserId) -> bool {
        self.clients
            .iter()
            .any(|client| client.is_admin && &client.user_id == user_id)
    }

    pub fn add_client(
        &mut self,
        demux_id: DemuxId,
        user_id: UserId,
        is_admin: bool,
        region_relation: RegionRelation,
        now: Instant,
    ) -> ClientStatus {
        let pending_client = NonParticipantClient {
            demux_id,
            user_id,
            is_admin,
            region_relation,
            next_server_to_client_data_rtp_seqnum: 1,
        };
        if self.blocked_users.contains(&pending_client.user_id) {
            debug!(
                "call: {} auto-denying blocked user {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            self.removed_clients.push(pending_client);
            ClientStatus::Blocked
        } else if is_admin
            || !self.new_clients_require_approval
            || self.approved_users.contains(&pending_client.user_id)
        {
            debug!(
                "call: {} adding client {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            if self.persist_approval_for_all_users_who_join {
                self.approved_users.insert(pending_client.user_id.clone());
            }
            self.promote_client(pending_client, now);
            ClientStatus::Active
        } else {
            debug!(
                "call: {} client {} requesting to join",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            // We use the same event to inform clients about changes in the pending list.
            self.will_add_or_remove_client(now);
            self.pending_clients.push(pending_client);
            ClientStatus::Pending
        }
    }

    fn will_add_or_remove_client(&mut self, now: Instant) {
        // An update message to clients about clients will be sent at the next tick().
        let increment = now.saturating_duration_since(self.client_added_or_removed);
        match self.clients.len() {
            0 => self.call_time.empty += increment,
            1 => self.call_time.solo += increment,
            2 => self.call_time.pair += increment,
            _ => self.call_time.many += increment,
        }
        self.client_added_or_removed = now;
    }

    fn promote_client(&mut self, pending_client: NonParticipantClient, now: Instant) {
        time_scope_us!("calling.call.promote_client");

        self.will_add_or_remove_client(now);

        let demux_id = pending_client.demux_id;
        self.clients.push(Client::new(
            pending_client,
            self.initial_target_send_rate,
            self.default_requested_max_send_rate,
            now,
        ));
        self.allocate_video_layers(
            demux_id,
            self.initial_target_send_rate,
            self.initial_target_send_rate,
            now,
        );
        // We may have to update the padding SSRCs because there can't be any padding SSRCs until two people join
        self.update_padding_ssrcs();
    }

    fn approve_pending_client(&mut self, demux_id: DemuxId, now: Instant) {
        if let Some(user_id) = self
            .pending_clients
            .iter()
            .find(|client| client.demux_id == demux_id)
            .map(|client| client.user_id.clone())
        {
            // Approve every client with the same user ID.
            let matching_pending_clients: Vec<_> =
                calling_common::drain_filter(&mut self.pending_clients, |client| {
                    client.user_id == user_id
                })
                .collect();
            for pending_client in matching_pending_clients {
                debug!(
                    "call: {} approving {}",
                    self.loggable_call_id(),
                    demux_id.as_u32()
                );
                self.promote_client(pending_client, now);
            }
            self.denied_users.remove(&user_id);
            self.approved_users.insert(user_id);
        }
    }

    fn deny_pending_client(&mut self, demux_id: DemuxId, now: Instant) {
        if let Some(user_id) = self
            .pending_clients
            .iter()
            .find(|client| client.demux_id == demux_id)
            .map(|client| client.user_id.clone())
        {
            self.will_add_or_remove_client(now);
            // Remove every client with the same user ID.
            let matching_pending_clients =
                calling_common::drain_filter(&mut self.pending_clients, |client| {
                    if client.user_id == user_id {
                        debug!(
                            "call: {} denying {}",
                            &self.loggable_call_id,
                            demux_id.as_u32()
                        );
                        true
                    } else {
                        false
                    }
                });
            self.removed_clients.extend(matching_pending_clients);

            if let Some(user_id) = self.denied_users.replace(user_id) {
                // Someone has denied this user before; elevate to a block to prevent them from
                // spamming the call.
                debug!(
                    "call: {} repeated deny elevated to block",
                    self.loggable_call_id(),
                );
                self.blocked_users.insert(user_id);
            }
        }
    }

    fn update_for_removed_clients(&mut self, removed_demux_ids: &[DemuxId], now: Instant) {
        self.reallocate_target_send_rates(now);
        self.update_padding_ssrcs();

        for client in &mut self.clients {
            for demux_id in removed_demux_ids {
                client.audio_forwarder_by_sender_demux_id.remove(demux_id);
                client.video_forwarder_by_sender_demux_id.remove(demux_id);
                client.data_forwarder_by_sender_demux_id.remove(demux_id);
                // Entries are removed from allocated_height_by_sender_demux_id in allocate_video_layers.
            }
        }

        self.key_frame_request_sent_by_ssrc
            .retain(|ssrc, _timestamp| !removed_demux_ids.contains(&DemuxId::from_ssrc(*ssrc)));
    }

    fn remove_client(&mut self, demux_id: DemuxId, now: Instant) -> Option<Client> {
        if let Some(index) = self
            .clients
            .iter()
            .position(|client| client.demux_id == demux_id)
        {
            self.will_add_or_remove_client(now);
            let removed_client = self.clients.swap_remove(index);
            self.update_for_removed_clients(&[demux_id], now);
            Some(removed_client)
        } else {
            None
        }
    }

    pub fn drop_client(&mut self, demux_id: DemuxId, now: Instant) {
        time_scope_us!("calling.call.drop_client");

        if self.remove_client(demux_id, now).is_some() {
            debug!(
                "call: {} dropping client {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
        } else if let Some(index) = self
            .pending_clients
            .iter()
            .position(|client| client.demux_id == demux_id)
        {
            debug!(
                "call: {} dropping pending client {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            self.will_add_or_remove_client(now);
            self.pending_clients.swap_remove(index);
        } else if let Some(index) = self
            .removed_clients
            .iter()
            .position(|client| client.demux_id == demux_id)
        {
            debug!(
                "call: {} dropping removed client {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            self.removed_clients.swap_remove(index);
        }
        self.lower_raised_hand(demux_id, now);
    }

    // Like `drop_client`, but keeps the client around in the `removed_clients` list until they leave.
    fn force_remove_client(&mut self, demux_id: DemuxId, now: Instant) {
        if let Some(client) = self.remove_client(demux_id, now) {
            debug!(
                "call: {} removing client {}",
                self.loggable_call_id(),
                demux_id.as_u32()
            );
            if !self
                .clients
                .iter()
                .any(|remaining_client| remaining_client.user_id == client.user_id)
            {
                // Reset the approval state *if* this was the user's only device in the call. We
                // only do this for lone devices because we don't want to get into a state where one
                // user has both a pending and an active client.
                debug!(
                    "call: {} approval revoked for {}",
                    self.loggable_call_id(),
                    demux_id.as_u32()
                );
                self.approved_users.remove(&client.user_id);
            }
            self.removed_clients.push(client.into());
            self.lower_raised_hand(demux_id, now);
        }
    }

    fn lower_raised_hand(&mut self, demux_id: DemuxId, now: Instant) {
        if let Some(raised_hands) = &mut self.raised_hands {
            // Set raise to false
            if let Some(index) = raised_hands.iter().position(|x| x.demux_id == demux_id) {
                if raised_hands[index].raise {
                    raised_hands[index].raise = false;
                    self.send_raised_hands_on_next_tick(now);
                }
            }
        }
    }

    fn send_raised_hands_on_next_tick(&mut self, now: Instant) {
        self.raised_hands_sent = now - RAISED_HANDS_MESSAGE_INTERVAL;
    }

    fn block_client(&mut self, demux_id: DemuxId, now: Instant) {
        if let Some(user_id) = self
            .clients
            .iter()
            .find(|client| client.demux_id == demux_id)
            .map(|client| client.user_id.clone())
        {
            self.will_add_or_remove_client(now);
            let removed_clients =
                calling_common::drain_filter(&mut self.clients, |client| client.user_id == user_id);
            let mut removed_demux_ids = Vec::new();
            for removed_client in removed_clients {
                debug!(
                    "call: {} removing blocked {}",
                    &self.loggable_call_id,
                    demux_id.as_u32()
                );
                removed_demux_ids.push(removed_client.demux_id);
                self.removed_clients.push(removed_client.into());
            }
            self.update_for_removed_clients(&removed_demux_ids, now);
            self.approved_users.remove(&user_id);
            self.blocked_users.insert(user_id);
        }
    }

    /// This updates the SSRCs that will be used to send padding.  We have to keep updating them
    /// because they have to be an SSRC of another client, which means there aren't any padding
    /// SSRCs until there are at least 2 clients in the call.
    fn update_padding_ssrcs(&mut self) {
        // We only send padding using an RTX SSRC. It doesn't matter which one.
        // The receiving client is configured to receive RTX for the base video 0
        // for each of the other clients in the call. So we have to pick one of those.
        // And the easiest one to pick is the RTX SSRC for the video base layer for
        // the given sender.demux_id.
        let padding_ssrc = |sender: &Client| Some(LayerId::Video0.to_rtx_ssrc(sender.demux_id));

        match self.clients.as_mut_slice() {
            [] => {
                // Nothing to update
            }
            [lonely] => {
                // Padding is not possible
                lonely.padding_ssrc = None;
            }
            [first, second, rest @ ..] => {
                // Just pick someone else.  The easiest way is to pick the first unless you're the first.
                first.padding_ssrc = padding_ssrc(second);
                second.padding_ssrc = padding_ssrc(first);
                for receiver in rest {
                    receiver.padding_ssrc = padding_ssrc(first);
                }
            }
        }
    }

    fn handle_raise_hand(
        &mut self,
        now: Instant,
        raise_hand: protos::device_to_sfu::RaiseHand,
        sender_demux_id: DemuxId,
    ) {
        if let (Some(raise), Some(seqnum)) = (raise_hand.raise, raise_hand.seqnum) {
            if self.raised_hands.is_none() {
                self.raised_hands = Some(Vec::new());
            }
            if let Some(raised_hands) = &mut self.raised_hands {
                let index = raised_hands
                    .iter()
                    .position(|x| x.demux_id == sender_demux_id);
                // Insert raise hand
                match index {
                    // Demux id in list
                    Some(index) => {
                        if let Some(current_seqnum) =
                            self.raised_hands_seqnums.get(&sender_demux_id)
                        {
                            // Modify raised hand when the seqnum is greater than the value in the list
                            if seqnum > *current_seqnum {
                                raised_hands.remove(index);
                                raised_hands.push(RaisedHand {
                                    demux_id: sender_demux_id,
                                    raise,
                                });
                                self.raised_hands_seqnums.insert(sender_demux_id, seqnum);
                                self.send_raised_hands_on_next_tick(now);
                            }
                        }
                    }
                    // Demux id not in list
                    None => {
                        // Add raised hand to end of list
                        raised_hands.push(RaisedHand {
                            demux_id: sender_demux_id,
                            raise,
                        });
                        self.raised_hands_seqnums.insert(sender_demux_id, seqnum);
                        self.send_raised_hands_on_next_tick(now);
                    }
                }
            }
        }
    }
    /// For a given packet from the sending client, determine what packets to
    /// send out to the other clients. This may include packets to forward
    /// and packets that update clients about active speaker changes and clients
    /// added and removed.  If the SSRC of the packet doesn't match the DemuxId,
    /// a UnauthorizedRtpSsrc error will be returned.
    /// If the DemuxId is unknown, an UnknownDemuxId error will be returned.
    pub fn handle_rtp(
        &mut self,
        sender_demux_id: DemuxId,
        incoming_rtp: rtp::Packet<&mut [u8]>,
        now: Instant,
    ) -> Result<Vec<RtpToSend>, Error> {
        if incoming_rtp.ssrc() == CLIENT_SERVER_DATA_SSRC
            && incoming_rtp.payload_type() == CLIENT_SERVER_DATA_PAYLOAD_TYPE
        {
            time_scope_us!("calling.call.handle_rtp.client_to_server_data");
            let proto = protos::DeviceToSfu::decode(incoming_rtp.payload())
                .map_err(|_| Error::InvalidClientToServerProtobuf)?;
            self.handle_device_to_sfu(proto, sender_demux_id, now)
        } else {
            self.handle_media_rtp(sender_demux_id, incoming_rtp, now)
        }
    }

    fn handle_device_to_sfu(
        &mut self,
        proto: protos::DeviceToSfu,
        sender_demux_id: DemuxId,
        now: Instant,
    ) -> Result<Vec<RtpToSend>, Error> {
        // Check for "Leave" before requiring that the demux ID is valid. We allow it for
        // pending and removed clients as well, and for some random other demux ID ignoring it
        // and just closing the connection is safe.
        if proto.leave.is_some() {
            // "Leave" is the only message we allow from pending and removed clients.
            info!(
                "call: {} removing client: {} (via RTP)",
                self.loggable_call_id(),
                sender_demux_id.as_u32()
            );
            self.drop_client(sender_demux_id, now);
            return Err(Error::Leave);
        }

        let sender_mrp_stream = &mut self
            .find_client_mut(sender_demux_id)
            .ok_or(Error::UnknownDemuxId(sender_demux_id))?
            .reliable_mrp_stream;
        let ready_protos = if let Some(header) = proto.mrp_header.as_ref() {
            match sender_mrp_stream.receive(&header.into(), proto) {
                Ok(ready_protos) => ready_protos,
                Err(e) => {
                    // received a malformed header, drop packet
                    event!("calling.call.handle_rtp.malformed_mrp_header");
                    return Err(Error::InvalidMrpHeader(e));
                }
            }
        } else {
            // process as an unreliable payload
            vec![proto]
        };

        // Snapshot this so we can get a mutable reference to the sender.
        let default_requested_max_send_rate = self.default_requested_max_send_rate;

        for proto in ready_protos {
            let sender = self
                .find_client_mut(sender_demux_id)
                .ok_or(Error::UnknownDemuxId(sender_demux_id))?;
            // And snapshot this so we can drop 'sender' after processing video requests.
            let sender_is_admin = sender.is_admin;
            // The client resends this periodically, so we don't want to do anything
            // if it didn't change.
            if proto.video_request != sender.video_request_proto {
                if let Some(video_request_proto) = proto.video_request {
                    sender.requested_height_by_demux_id = video_request_proto
                        .requests
                        .iter()
                        .filter_map(|request| {
                            let raw_height = request.height?;
                            let height = VideoHeight::from(raw_height as u16);

                            if let Some(raw_demux_id) = request.demux_id {
                                let demux_id = DemuxId::try_from(raw_demux_id).ok()?;
                                Some((demux_id, height))
                            } else {
                                None
                            }
                        })
                        .collect();
                    sender.requested_max_send_rate = video_request_proto
                        .max_kbps
                        .map(|kbps| DataRate::from_kbps(kbps as u64))
                        .unwrap_or(default_requested_max_send_rate);
                    sender.active_speaker_height = video_request_proto
                        .active_speaker_height
                        .map(|height| height as u16)
                        .unwrap_or(0);
                    sender.video_request_proto = Some(video_request_proto);
                    // We reallocate immediately to make a more pleasant expereience for the user
                    // (no extra delay for selecting a higher resolution or requesting a new max send rate)
                    let target_send_rate = sender.target_send_rate;
                    let min_target_send_rate = sender.min_target_send_rate();
                    self.allocate_video_layers(
                        sender_demux_id,
                        target_send_rate,
                        min_target_send_rate,
                        now,
                    );
                }
            }

            if !proto.approve.is_empty()
                || !proto.deny.is_empty()
                || !proto.remove.is_empty()
                || !proto.block.is_empty()
            {
                if sender_is_admin {
                    fn record_malformed_admin_action() {
                        event!("calling.call.handle_rtp.malformed_admin_action");
                    }

                    for action in proto.approve {
                        if let Some(demux_id) = action
                            .target_demux_id
                            .and_then(|demux_id| DemuxId::try_from(demux_id).ok())
                        {
                            self.approve_pending_client(demux_id, now);
                        } else {
                            record_malformed_admin_action();
                        }
                    }

                    for action in proto.deny {
                        if let Some(demux_id) = action
                            .target_demux_id
                            .and_then(|demux_id| DemuxId::try_from(demux_id).ok())
                        {
                            self.deny_pending_client(demux_id, now);
                        } else {
                            record_malformed_admin_action();
                        }
                    }

                    for action in proto.remove {
                        if let Some(demux_id) = action
                            .target_demux_id
                            .and_then(|demux_id| DemuxId::try_from(demux_id).ok())
                        {
                            self.force_remove_client(demux_id, now);
                        } else {
                            record_malformed_admin_action();
                        }
                    }

                    for action in proto.block {
                        if let Some(demux_id) = action
                            .target_demux_id
                            .and_then(|demux_id| DemuxId::try_from(demux_id).ok())
                        {
                            self.block_client(demux_id, now);
                        } else {
                            record_malformed_admin_action();
                        }
                    }
                } else {
                    event!("calling.call.handle_rtp.non_admin_sent_admin_action");
                }
            }

            if let Some(raise_hand) = proto.raise_hand {
                self.handle_raise_hand(now, raise_hand, sender_demux_id);
            }
        }

        // There's nothing to forward
        Ok(vec![])
    }

    fn handle_media_rtp(
        &mut self,
        sender_demux_id: DemuxId,
        incoming_rtp: rtp::Packet<&mut [u8]>,
        now: Instant,
    ) -> Result<Vec<RtpToSend>, Error> {
        let sender = self
            .find_client_mut(sender_demux_id)
            .ok_or(Error::UnknownDemuxId(sender_demux_id))?;

        // Make sure to do this before processing audio level, etc.
        // Otherwise someone could fake the SSRC to change active speaker and that sort of thing.
        let authorized_sender_demux_id = DemuxId::from_ssrc(incoming_rtp.ssrc());
        if authorized_sender_demux_id != sender_demux_id {
            return Err(Error::UnauthorizedRtpSsrc(
                authorized_sender_demux_id,
                sender_demux_id,
            ));
        }

        let incoming_rtp = incoming_rtp.borrow();
        let incoming_vp8 = if incoming_rtp.is_vp8() {
            time_scope_us!("calling.call.handle_rtp.vp8_header");
            if let Some(incoming_vp8) = sender
                .parse_vp8_header_and_update_incoming_video_rate_and_resolution(
                    &incoming_rtp,
                    now,
                )?
            {
                Some(incoming_vp8)
            } else {
                return Ok(vec![]);
            }
        } else {
            None
        };

        let mut rtp_to_send = vec![];
        if let Some(audio_level) = incoming_rtp.audio_level {
            time_scope_us!("calling.call.handle_rtp.audio_level");
            sender.incoming_audio_levels.push(audio_level);
            // Active speaker is recalculated in tick()
        }

        let layer_id = LayerId::from_ssrc(incoming_rtp.ssrc()).ok_or(Error::InvalidRtpLayerId)?;

        time_scope_us!("calling.call.handle_rtp.forwarding");

        for receiver in &mut self.clients {
            if receiver.demux_id == sender_demux_id {
                // Don't send to yourself.
                continue;
            }
            if let Some(rtp_to_forward) = match layer_id {
                LayerId::Audio => {
                    let is_silence = incoming_rtp.audio_level == Some(0);
                    if is_silence {
                        None
                    } else {
                        receiver.forward_audio_rtp(&incoming_rtp)
                    }
                }
                LayerId::RtpData => receiver.forward_data_rtp(&incoming_rtp),
                LayerId::Video0 | LayerId::Video1 | LayerId::Video2 => {
                    receiver.forward_video_rtp(&incoming_rtp, incoming_vp8.as_ref())
                }
            } {
                rtp_to_send.push((receiver.demux_id, rtp_to_forward));
            }
        }
        Ok(rtp_to_send)
    }

    /// Update state that only needs to be updated regularly, such as
    /// incoming data rates, send rate allocations, and the active speaker.
    /// Send packets to clients that should either be delayed or be sent regularly,
    /// such as key frame requests and active speaker changes.
    pub fn tick(&mut self, now: Instant) -> (Vec<RtpToSend>, Vec<KeyFrameRequestToSend>) {
        time_scope_us!("calling.call.tick");

        self.approved_users.tick();

        for sender in &mut self.clients {
            sender.incoming_video0.rate_tracker.update(now);
            sender.incoming_video1.rate_tracker.update(now);
            sender.incoming_video2.rate_tracker.update(now);
        }

        let mut new_active_speaker: Option<DemuxId> = None;
        if now > self.active_speaker_calculated + ACTIVE_SPEAKER_CALCULATION_INTERVAL {
            time_scope_us!("calling.call.tick.calculate_active_speaker");

            self.active_speaker_calculated = now;
            new_active_speaker = self.calculate_active_speaker(now);
            if new_active_speaker.is_some() {
                trace!("  active speaker changed");
                trace!("  send rtp packet with active speaker change to all clients in the sender's call");
                // update proto is sent down below
            }
        }

        let client_was_added_or_removed = self.client_added_or_removed > self.clients_update_sent;
        if client_was_added_or_removed {
            self.clients_update_sent = now;
        }

        // A change to the layer rate or resolution may impact how the receiver allocates the target sent rate.
        // So can a change in active speaker.
        // So we should reallocate after changing the incoming rates above and active speaker above.
        self.reallocate_target_send_rates_if_its_been_too_long(now);

        // Do this after reallocation so it has the latest info about what is being forwarded.
        let mut rtp_to_send = vec![];
        self.send_update_proto_to_participating_clients(
            client_was_added_or_removed,
            new_active_speaker.is_some(),
            &mut rtp_to_send,
            now,
        );
        self.send_update_proto_to_pending_clients(client_was_added_or_removed, &mut rtp_to_send);
        self.send_update_proto_to_removed_clients(&mut rtp_to_send, now);
        self.send_raised_hands_proto_to_clients(&mut rtp_to_send, now);

        // Reallocation can change what key frames to send, so we should do this after reallocating.
        let mut key_frame_requests_to_send = self.send_key_frame_requests_if_its_been_too_long(now);

        if let Some(active_speaker_id) = new_active_speaker {
            let max_requested_active_speaker_height = self
                .clients
                .iter()
                // Don't request key frames for yourself
                .filter(|client| client.demux_id != active_speaker_id)
                .map(|client| client.active_speaker_height)
                .max()
                .unwrap_or(0);

            let active_speaker = self
                .find_client(active_speaker_id)
                .expect("active speaker is a client");

            if let Some(active_speaker_layer0_height) = active_speaker.incoming_video0.height {
                if max_requested_active_speaker_height > active_speaker_layer0_height.as_u16() {
                    key_frame_requests_to_send.extend_from_slice(&[
                        (
                            active_speaker_id,
                            rtp::KeyFrameRequest {
                                ssrc: LayerId::Video1.to_ssrc(active_speaker_id),
                            },
                        ),
                        (
                            active_speaker_id,
                            rtp::KeyFrameRequest {
                                ssrc: LayerId::Video2.to_ssrc(active_speaker_id),
                            },
                        ),
                    ]);
                } else {
                    // The smallest layer is good enough for everyone
                }
            } else {
                trace!("No video from the active speaker. Not requesting key frames.");
            }
        }

        self.send_mrp_updates(&mut rtp_to_send, now);

        (rtp_to_send, key_frame_requests_to_send)
    }

    /// Adjust the target send rate for the given client according to what congestion control has
    /// calculated.
    pub fn set_target_send_rate(
        &mut self,
        receiver_demux_id: DemuxId,
        new_target_send_rate: DataRate,
        now: Instant,
    ) -> Result<(), Error> {
        let receiver = self
            .find_client_mut(receiver_demux_id)
            .ok_or(Error::UnknownDemuxId(receiver_demux_id))?;
        receiver.target_send_rate = new_target_send_rate;

        if now > receiver.next_min_target_generation_update_time {
            receiver.old_generation_min_target_send_rate =
                receiver.current_generation_min_target_send_rate;
            receiver.current_generation_min_target_send_rate = new_target_send_rate;
            receiver.next_min_target_generation_update_time =
                now + MIN_TARGET_SEND_RATE_GENERATION_INTERVAL;
        } else if new_target_send_rate < receiver.current_generation_min_target_send_rate {
            receiver.current_generation_min_target_send_rate = new_target_send_rate;
        }
        Ok(())
    }

    pub fn set_outgoing_queue_drain_rate(
        &mut self,
        receiver_demux_id: DemuxId,
        outgoing_queue_drain_rate: DataRate,
    ) -> Result<(), Error> {
        let receiver = self
            .find_client_mut(receiver_demux_id)
            .ok_or(Error::UnknownDemuxId(receiver_demux_id))?;
        receiver.outgoing_queue_drain_rate = outgoing_queue_drain_rate;
        Ok(())
    }

    pub fn set_connection_rates(
        &mut self,
        receiver_demux_id: DemuxId,
        connection_rates: ConnectionRates,
    ) -> Result<(), Error> {
        let receiver = self
            .find_client_mut(receiver_demux_id)
            .ok_or(Error::UnknownDemuxId(receiver_demux_id))?;
        receiver.connection_rates = connection_rates;
        Ok(())
    }

    pub fn get_send_rate_allocation_info(
        &self,
    ) -> impl Iterator<Item = SendRateAllocationInfo> + '_ {
        self.clients.iter().map(|client| SendRateAllocationInfo {
            demux_id: client.demux_id,
            padding_ssrc: client.padding_ssrc,
            target_send_rate: client.target_send_rate,
            requested_base_rate: client.requested_base_rate,
            ideal_send_rate: client.ideal_send_rate,
        })
    }

    fn reallocate_target_send_rates_if_its_been_too_long(&mut self, now: Instant) {
        let receivers: Vec<_> = self
            .clients
            .iter()
            .filter_map(|receiver| {
                if now > (receiver.send_rate_allocated + SEND_RATE_REALLOCATION_INTERVAL) {
                    Some((
                        receiver.demux_id,
                        receiver.target_send_rate,
                        receiver.min_target_send_rate(),
                    ))
                } else {
                    None
                }
            })
            .collect();

        for (receiver_demux_id, target_send_rate, min_target_send_rate) in receivers {
            self.allocate_video_layers(
                receiver_demux_id,
                target_send_rate,
                min_target_send_rate,
                now,
            );
        }
    }

    fn reallocate_target_send_rates(&mut self, now: Instant) {
        let receivers: Vec<_> = self
            .clients
            .iter()
            .map(|client| {
                (
                    client.demux_id,
                    client.target_send_rate,
                    client.min_target_send_rate(),
                )
            })
            .collect();
        for (receiver_demux_id, target_send_rate, min_target_send_rate) in receivers {
            self.allocate_video_layers(
                receiver_demux_id,
                target_send_rate,
                min_target_send_rate,
                now,
            );
        }
    }

    /// Determines which video layers should be forwarded from other clients to
    /// `receiver_demux_id` based on what congestion control calculated.
    fn allocate_video_layers(
        &mut self,
        receiver_demux_id: DemuxId,
        new_target_send_rate: DataRate,
        min_target_send_rate: DataRate,
        now: Instant,
    ) {
        let receiver = self
            .find_client(receiver_demux_id)
            .expect("Client exists before trying to allocate target send rate");

        // We have to collect these because we can't get a mutable ref to the receiver while getting
        // immutable refs to the senders.
        let allocatable_videos: Vec<AllocatableVideo> = self
            .clients
            .iter()
            .filter_map(|sender| {
                if sender.demux_id == receiver_demux_id {
                    // Don't send video to yourself
                    return None;
                }

                let mut requested_height = receiver
                    .requested_height_by_demux_id
                    .get(&sender.demux_id)
                    .copied()
                    .unwrap_or_else(|| VideoHeight::from(1));

                // Override the requested height for the active speaker to support early requests
                // from the SFU for higher video layers before the client's UI updates.
                if Some(sender.demux_id) == self.active_speaker_id
                    && receiver.active_speaker_height > requested_height.as_u16()
                {
                    requested_height = VideoHeight::from(receiver.active_speaker_height);
                }

                let allocated_layer_index = receiver
                    .video_forwarder_by_sender_demux_id
                    .get(&sender.demux_id)
                    .and_then(|f| f.forwarding_ssrc())
                    .and_then(LayerId::layer_index_from_ssrc);

                Some(AllocatableVideo {
                    sender_demux_id: sender.demux_id,
                    layers: [
                        sender.incoming_video0.as_allocatable_layer(),
                        sender.incoming_video1.as_allocatable_layer(),
                        sender.incoming_video2.as_allocatable_layer(),
                    ],
                    requested_height,
                    allocated_layer_index,
                    interesting: sender.became_active_speaker,
                })
            })
            .collect();
        let receiver = self.find_client_mut(receiver_demux_id).unwrap();

        // We have to collect these because we can't get a mutable ref to the receiver while getting
        // immutable refs to the senders.
        let sender_demux_ids: Vec<DemuxId> = allocatable_videos
            .iter()
            .map(|video| video.sender_demux_id)
            .filter(|sender_demux_id| *sender_demux_id != receiver.demux_id)
            .collect();
        let requested_base_rate =
            requested_base_rate(&allocatable_videos, receiver.requested_max_send_rate);
        let ideal_send_rate =
            ideal_send_rate(&allocatable_videos, receiver.requested_max_send_rate);

        let allocated_video_by_sender_demux_id = allocate_send_rate(
            new_target_send_rate,
            min_target_send_rate,
            ideal_send_rate,
            receiver.outgoing_queue_drain_rate,
            allocatable_videos,
        );
        let allocated_send_rate = allocated_video_by_sender_demux_id
            .values()
            .map(|allocated| allocated.rate)
            .sum();

        receiver.allocated_height_by_sender_demux_id.clear();

        for sender_demux_id in sender_demux_ids {
            let desired_incoming_ssrc = allocated_video_by_sender_demux_id
                .get(&sender_demux_id)
                .map(|allocated_video| {
                    receiver
                        .allocated_height_by_sender_demux_id
                        .insert(sender_demux_id, allocated_video.height);

                    let layer_id =
                        LayerId::from_video_layer_index(allocated_video.layer_index).unwrap();
                    layer_id.to_ssrc(allocated_video.sender_demux_id)
                });
            let forwarder = receiver
                .video_forwarder_by_sender_demux_id
                .entry(sender_demux_id)
                .or_insert_with(|| {
                    let outgoing_ssrc = LayerId::Video0.to_ssrc(sender_demux_id);
                    Vp8SimulcastRtpForwarder::new(outgoing_ssrc)
                });
            forwarder.set_desired_ssrc(desired_incoming_ssrc);
        }

        receiver.target_send_rate = new_target_send_rate;
        receiver.requested_base_rate = requested_base_rate;
        receiver.ideal_send_rate = ideal_send_rate;
        receiver.allocated_send_rate = allocated_send_rate;
        receiver.send_rate_allocated = now;
    }

    pub fn handle_key_frame_requests(
        &mut self,
        requester_id: DemuxId,
        key_frame_requests: &[rtp::KeyFrameRequest],
        now: Instant,
    ) -> Vec<(DemuxId, rtp::KeyFrameRequest)> {
        let requester = self.find_client_mut(requester_id);
        if requester.is_none() {
            return vec![];
        }
        let requester = requester.unwrap();

        for key_frame_request in key_frame_requests {
            // This might not send them immediately because we might have just sent one
            // and this still has to respect throttling.
            let video_sender_demux_id = DemuxId::from_ssrc(key_frame_request.ssrc);
            let video_forwarder = requester
                .video_forwarder_by_sender_demux_id
                .get_mut(&video_sender_demux_id);
            if let Some(video_forwarder) = video_forwarder {
                event!("calling.rtcp.pli.incoming");
                video_forwarder.set_needs_key_frame();
            }
        }
        self.send_key_frame_requests_if_its_been_too_long(now)
    }

    fn find_client(&self, demux_id: DemuxId) -> Option<&Client> {
        self.clients
            .iter()
            .find(|client| client.demux_id == demux_id)
    }

    fn find_client_mut(&mut self, demux_id: DemuxId) -> Option<&mut Client> {
        self.clients
            .iter_mut()
            .find(|client| client.demux_id == demux_id)
    }

    fn send_update_proto_to_participating_clients(
        &mut self,
        client_was_added_or_removed: bool,
        active_speaker_just_changed: bool,
        rtp_to_send: &mut Vec<RtpToSend>,
        now: Instant,
    ) {
        let mut update = protos::SfuToDevice::default();

        if client_was_added_or_removed {
            // The fields aren't used by the client, so they are None.
            update.device_joined_or_left =
                Some(protos::sfu_to_device::DeviceJoinedOrLeft::default());
        }

        if active_speaker_just_changed
            || update.device_joined_or_left.is_some()
            || (now >= self.active_speaker_update_sent + self.active_speaker_message_interval)
        {
            if let Some(demux_id) = self.active_speaker_id {
                update.speaker = Some(protos::sfu_to_device::Speaker {
                    demux_id: Some(demux_id.as_u32()),
                });
            }
            self.active_speaker_update_sent = now;
        }

        let send_stats = now >= self.stats_update_sent + STATS_MESSAGE_INTERVAL;
        if update.device_joined_or_left.is_some() || update.speaker.is_some() || send_stats {
            let raw_demux_ids: Vec<u32> = self
                .clients
                .iter()
                .map(|client| client.demux_id.as_u32())
                .collect();

            for client in &mut self.clients {
                let (demux_ids_with_video, allocated_heights) = client
                    .video_forwarder_by_sender_demux_id
                    .iter()
                    .filter_map(|(demux_id, forwarder)| {
                        // We don't want the clients to draw an empty box when a key frame might be coming soon,
                        // so we count it as forwarding if we're still waiting for a key frame.
                        if forwarder.forwarding_ssrc().is_some()
                            || forwarder.needs_key_frame().is_some()
                        {
                            Some((
                                demux_id.as_u32(),
                                client
                                    .allocated_height_by_sender_demux_id
                                    .get(demux_id)
                                    .unwrap_or(&VideoHeight::from(0))
                                    .as_u16() as u32,
                            ))
                        } else {
                            None
                        }
                    })
                    .unzip();

                update.current_devices = Some(protos::sfu_to_device::CurrentDevices {
                    all_demux_ids: if cfg!(test) {
                        raw_demux_ids.clone()
                    } else {
                        // Clients don't make use of this information, so we leave it out when not
                        // running tests. Note that DeviceAddedOrRemoved is currently used to signal
                        // updates to both the active clients list and the pending clients list, so
                        // any changes that make use of this field should consider adding a second
                        // field for pending devices. The lists may also need to be *sent* to
                        // pending devices as well, since they also maintain peek info.
                        vec![]
                    },
                    demux_ids_with_video,
                    allocated_heights,
                });
                if send_stats {
                    update.stats = Some(protos::sfu_to_device::Stats {
                        target_send_rate_kbps: Some(client.target_send_rate.as_kbps() as u32),
                        ideal_send_rate_kbps: Some(client.ideal_send_rate.as_kbps() as u32),
                        allocated_send_rate_kbps: Some(client.allocated_send_rate.as_kbps() as u32),
                    });
                }

                let update_rtp = Self::encode_sfu_to_device_update(
                    &update,
                    &mut client.next_server_to_client_data_rtp_seqnum,
                );
                rtp_to_send.push((client.demux_id, update_rtp))
            }

            if send_stats {
                self.stats_update_sent = now;
            }
        }
    }

    fn send_update_proto_to_pending_clients(
        &mut self,
        client_was_added_or_removed: bool,
        rtp_to_send: &mut Vec<RtpToSend>,
    ) {
        if self.pending_clients.is_empty() || !client_was_added_or_removed {
            return;
        }

        let update = protos::SfuToDevice {
            device_joined_or_left: Some(protos::sfu_to_device::DeviceJoinedOrLeft::default()),
            ..Default::default()
        };

        for pending_client in &mut self.pending_clients {
            rtp_to_send.push((
                pending_client.demux_id,
                Self::encode_sfu_to_device_update(
                    &update,
                    &mut pending_client.next_server_to_client_data_rtp_seqnum,
                ),
            ))
        }
    }

    fn send_update_proto_to_removed_clients(
        &mut self,
        rtp_to_send: &mut Vec<RtpToSend>,
        now: Instant,
    ) {
        if self.removed_clients.is_empty()
            || self.removed_clients_update_sent + REMOVED_CLIENTS_UPDATE_INTERVAL > now
        {
            return;
        }
        self.removed_clients_update_sent = now;

        let update = protos::SfuToDevice {
            removed: Some(protos::sfu_to_device::Removed {}),
            ..Default::default()
        };

        for removed_client in &mut self.removed_clients {
            rtp_to_send.push((
                removed_client.demux_id,
                Self::encode_sfu_to_device_update(
                    &update,
                    &mut removed_client.next_server_to_client_data_rtp_seqnum,
                ),
            ))
        }
    }

    fn send_raised_hands_proto_to_clients(
        &mut self,
        rtp_to_send: &mut Vec<RtpToSend>,
        now: Instant,
    ) {
        if now >= self.raised_hands_sent + RAISED_HANDS_MESSAGE_INTERVAL {
            if let Some(raised_hands) = &self.raised_hands {
                // Generate a list of demux ids and seqnums where the raise value is true
                let (demux_ids, seqnums) = raised_hands
                    .iter()
                    .filter(|h| h.raise)
                    .map(|h| {
                        (
                            h.demux_id.as_u32(),
                            self.raised_hands_seqnums.get(&h.demux_id).unwrap_or(&0),
                        )
                    })
                    .unzip();

                let mut update = protos::SfuToDevice {
                    raised_hands: Some(protos::sfu_to_device::RaisedHands {
                        demux_ids,
                        seqnums,
                        target_seqnum: Some(0),
                    }),
                    ..Default::default()
                };

                for client in &mut self.clients {
                    // Set the target_seqnum of the client
                    let target_seqnum = self
                        .raised_hands_seqnums
                        .get(&client.demux_id)
                        .unwrap_or(&0);
                    update.raised_hands.as_mut().unwrap().target_seqnum = Some(*target_seqnum);

                    let update_rtp = Self::encode_sfu_to_device_update(
                        &update,
                        &mut client.next_server_to_client_data_rtp_seqnum,
                    );
                    rtp_to_send.push((client.demux_id, update_rtp))
                }

                self.raised_hands_sent = now;
            }
        }
    }

    /// Preps and appends MRP acks and retries to clients in the call
    fn send_mrp_updates(&mut self, rtp_to_send: &mut Vec<RtpToSend>, now: Instant) {
        let unwrapped_now = now.into();
        for client in &mut self.clients {
            let client_demux_id = client.demux_id;
            let _ = client.reliable_mrp_stream.try_send_ack(|header| {
                let ack = protos::SfuToDevice {
                    mrp_header: Some(header.into()),
                    ..Default::default()
                };
                let update_rtp = Self::encode_reliable_sfu_to_device(
                    &ack,
                    &mut client.next_server_to_client_data_rtp_seqnum,
                );
                rtp_to_send.push((client_demux_id, update_rtp));
                Ok(())
            });
            let _ = client.reliable_mrp_stream.try_resend(unwrapped_now, |pkt| {
                let update_rtp = Self::encode_reliable_sfu_to_device(
                    pkt,
                    &mut client.next_server_to_client_data_rtp_seqnum,
                );
                rtp_to_send.push((client_demux_id, update_rtp));
                Ok(unwrapped_now + MRP_SEND_TIMEOUT_INTERVAL.into())
            });
        }
    }

    fn encode_sfu_to_device_update(
        update: &protos::SfuToDevice,
        next_server_to_client_data_rtp_seqnum: &mut rtp::FullSequenceNumber,
    ) -> rtp::Packet<Vec<u8>> {
        Self::encode_sfu_to_device_inner(
            update,
            next_server_to_client_data_rtp_seqnum,
            CLIENT_SERVER_DATA_PAYLOAD_TYPE,
        )
    }

    fn encode_reliable_sfu_to_device(
        update: &protos::SfuToDevice,
        next_server_to_client_data_rtp_seqnum: &mut rtp::FullSequenceNumber,
    ) -> rtp::Packet<Vec<u8>> {
        Self::encode_sfu_to_device_inner(
            update,
            next_server_to_client_data_rtp_seqnum,
            CLIENT_SERVER_DATA_PAYLOAD_TYPE,
        )
    }

    fn encode_sfu_to_device_inner(
        update: &protos::SfuToDevice,
        next_server_to_client_data_rtp_seqnum: &mut rtp::FullSequenceNumber,
        payload_type: rtp::PayloadType,
    ) -> rtp::Packet<Vec<u8>> {
        let seqnum: rtp::FullSequenceNumber = *next_server_to_client_data_rtp_seqnum;
        *next_server_to_client_data_rtp_seqnum += 1;
        let timestamp = seqnum as rtp::TruncatedTimestamp;
        rtp::Packet::with_empty_tag(
            payload_type,
            seqnum,
            timestamp,
            CLIENT_SERVER_DATA_SSRC,
            None,
            None,
            &update.encode_to_vec(),
        )
    }

    fn calculate_active_speaker(&mut self, now: Instant) -> Option<DemuxId> {
        let first = self.clients.first()?;
        let mut most_active = self
            .active_speaker_id
            .and_then(|demux_id| self.find_client(demux_id))
            .unwrap_or(first);

        for contender in &self.clients {
            if contender.demux_id != most_active.demux_id
                && contender
                    .incoming_audio_levels
                    .more_active_than_most_active(&most_active.incoming_audio_levels)
            {
                most_active = contender;
            }
        }

        let most_active_demux_id = most_active.demux_id;
        if self.active_speaker_id != Some(most_active_demux_id) {
            self.find_client_mut(most_active_demux_id)
                .unwrap()
                .became_active_speaker = Some(now);
            self.active_speaker_id = Some(most_active_demux_id);
            Some(most_active_demux_id)
        } else {
            None
        }
    }

    // All kinds of things can happen that trigger key frame requests to be needed:
    // - Video requests from clients
    // - Incoming bitrates changing
    // - Outgoing target bitrates changing
    // - Time passing
    // - Receivers sending key frame requests (PLIs)
    // Rather than try to catch all those cases, just call this occasionally.
    // Plus, key frame requests can be dropped, so we need to resend them occasionally.
    pub fn send_key_frame_requests_if_its_been_too_long(
        &mut self,
        now: Instant,
    ) -> Vec<(DemuxId, rtp::KeyFrameRequest)> {
        if now < self.key_frame_requests_sent + KEY_FRAME_REQUEST_CALCULATION_INTERVAL {
            // We sent key frame requests recently. Wait to resend/recalculate them.
            return vec![];
        }

        let mut desired_incoming_ssrcs: HashSet<rtp::Ssrc> = HashSet::new();
        for receiver in &mut self.clients {
            for video_forwarder in receiver.video_forwarder_by_sender_demux_id.values() {
                if let Some(desired_incoming_ssrc) = video_forwarder.needs_key_frame() {
                    desired_incoming_ssrcs.insert(desired_incoming_ssrc);
                }
            }
        }

        let key_frame_requests: Vec<(DemuxId, rtp::KeyFrameRequest)> = desired_incoming_ssrcs
            .into_iter()
            .filter_map(|desired_incoming_ssrc| {
                let sent = self
                    .key_frame_request_sent_by_ssrc
                    .get(&desired_incoming_ssrc)
                    .copied();
                let sent_recently =
                    sent.is_some() && now < (sent.unwrap() + KEY_FRAME_REQUEST_RESEND_INTERVAL);
                if sent_recently {
                    // If we sent a key frame for this SSRC recently, wait to resend one.
                    None
                } else {
                    let video_height = self.incoming_video_height(desired_incoming_ssrc);
                    if video_height.unwrap_or_default() > VideoHeight::from(720)
                        && sent.is_some()
                        && now < (sent.unwrap() + HIGH_RES_KEY_FRAME_REQUEST_RESEND_INTERVAL)
                    {
                        None
                    } else {
                        self.key_frame_request_sent_by_ssrc
                            .insert(desired_incoming_ssrc, now);
                        Some((
                            DemuxId::from_ssrc(desired_incoming_ssrc),
                            rtp::KeyFrameRequest {
                                ssrc: desired_incoming_ssrc,
                            },
                        ))
                    }
                }
            })
            .collect();

        if !key_frame_requests.is_empty() {
            event!("calling.rtcp.pli.outgoing", key_frame_requests.len());
        }

        self.key_frame_requests_sent = now;
        key_frame_requests
    }

    fn incoming_video_height(&self, ssrc: rtp::Ssrc) -> Option<VideoHeight> {
        let client = self.find_client(DemuxId::from_ssrc(ssrc))?;
        match LayerId::from_ssrc(ssrc)? {
            LayerId::Video0 => client.incoming_video0.height,
            LayerId::Video1 => client.incoming_video1.height,
            LayerId::Video2 => client.incoming_video2.height,
            LayerId::Audio | LayerId::RtpData => None,
        }
    }

    /// Get the DemuxIds and opaque user IDs for each client.  These are needed for signaling.
    pub fn get_client_ids(&self) -> Vec<(DemuxId, UserId)> {
        self.clients
            .iter()
            .map(|client| (client.demux_id, client.user_id.clone()))
            .collect()
    }

    /// Get the DemuxIds and user IDs for each pending client.  These are needed for signaling.
    pub fn get_pending_client_ids(&self, include_user_ids: bool) -> Vec<(DemuxId, Option<UserId>)> {
        self.pending_clients
            .iter()
            .map(|client| {
                (
                    client.demux_id,
                    if include_user_ids {
                        Some(client.user_id.clone())
                    } else {
                        None
                    },
                )
            })
            .collect()
    }

    pub fn get_stats(&self) -> CallStats {
        CallStats {
            loggable_call_id: self.loggable_call_id.clone(),
            clients: self.clients.iter().map(Client::get_stats).collect(),
        }
    }
}

/// Enough information to send RTP data messages to and from the client, but not do any forwarding.
struct NonParticipantClient {
    // Immutable
    demux_id: DemuxId,
    user_id: UserId,
    is_admin: bool,
    region_relation: RegionRelation,

    // Update with each proto send from server to client
    next_server_to_client_data_rtp_seqnum: rtp::FullSequenceNumber,
}

impl From<Client> for NonParticipantClient {
    fn from(client: Client) -> Self {
        Self {
            demux_id: client.demux_id,
            user_id: client.user_id,
            is_admin: client.is_admin,
            region_relation: client.region_relation,

            next_server_to_client_data_rtp_seqnum: client.next_server_to_client_data_rtp_seqnum,
        }
    }
}

impl From<&protos::MrpHeader> for mrp::MrpHeader {
    fn from(value: &protos::MrpHeader) -> Self {
        Self {
            ack_num: value.ack_num,
            seqnum: value.seqnum,
        }
    }
}

impl From<mrp::MrpHeader> for protos::MrpHeader {
    fn from(value: mrp::MrpHeader) -> Self {
        Self {
            ack_num: value.ack_num,
            seqnum: value.seqnum,
        }
    }
}

enum VideoHeader {
    VP8(vp8::ParsedHeader),
    DependencyDescriptor(rtp::DependencyDescriptor),
}

impl VideoHeader {
    fn is_key_frame(&self) -> bool {
        match self {
            VideoHeader::VP8(header) => header.is_key_frame,
            VideoHeader::DependencyDescriptor(descriptor) => descriptor.is_key_frame,
        }
    }

    fn resolution(&self) -> Option<PixelSize> {
        match self {
            VideoHeader::VP8(header) => header.resolution,
            VideoHeader::DependencyDescriptor(descriptor) => descriptor.resolution,
        }
    }

    fn picture_id(&self) -> Option<vp8::TruncatedPictureId> {
        match self {
            VideoHeader::VP8(header) => header.picture_id,
            VideoHeader::DependencyDescriptor(_) => None,
        }
    }

    fn tl0_pic_idx(&self) -> Option<vp8::TruncatedTl0PicIdx> {
        match self {
            VideoHeader::VP8(header) => header.tl0_pic_idx,
            VideoHeader::DependencyDescriptor(_) => None,
        }
    }

    fn truncated_frame_number(&self) -> Option<u16> {
        match self {
            VideoHeader::VP8(_) => None,
            VideoHeader::DependencyDescriptor(descriptor) => descriptor.truncated_frame_number,
        }
    }
}

impl From<vp8::ParsedHeader> for VideoHeader {
    fn from(value: vp8::ParsedHeader) -> Self {
        Self::VP8(value)
    }
}

impl From<rtp::DependencyDescriptor> for VideoHeader {
    fn from(value: rtp::DependencyDescriptor) -> Self {
        Self::DependencyDescriptor(value)
    }
}

/// The per-client state
struct Client {
    // Immutable
    demux_id: DemuxId,
    user_id: UserId,
    is_admin: bool,
    region_relation: RegionRelation,

    // Updated by incoming video packets
    incoming_video0: IncomingVideoState,
    incoming_video1: IncomingVideoState,
    incoming_video2: IncomingVideoState,
    video_rotation: VideoRotation,

    // Updated by incoming audio packets
    incoming_audio_levels: audio::LevelsTracker,
    became_active_speaker: Option<Instant>,

    // Updated by incoming video requests
    video_request_proto: Option<protos::device_to_sfu::VideoRequestMessage>,
    requested_height_by_demux_id: HashMap<DemuxId, VideoHeight>,
    active_speaker_height: u16,

    // Updated by Call::set_target_send_rate
    target_send_rate: DataRate,
    current_generation_min_target_send_rate: DataRate,
    old_generation_min_target_send_rate: DataRate,
    next_min_target_generation_update_time: Instant,

    // Updated by Call::set_outgoing_queue_drain_rate
    outgoing_queue_drain_rate: DataRate,
    requested_max_send_rate: DataRate,
    send_rate_allocated: Instant,

    // Updated by send rate allocation, which is affected by
    // incoming video requests, target send rate,
    // incoming packets, and calls to tick().
    // requested_base_rate is the sum of the rates of the requested base layers.
    // Like ideal_send_rate, it's capped by max_requested_send_rate.
    requested_base_rate: DataRate,
    ideal_send_rate: DataRate,
    allocated_send_rate: DataRate,

    // Updated during sfu tick
    connection_rates: ConnectionRates,

    // Updated by Call::update_padding_ssrc()
    padding_ssrc: Option<rtp::Ssrc>,

    // Updated by incoming video requests, target send rate,
    // incoming packets, and calls to tick().
    // Note: The following is n^2 memory usage
    // (where n is the number of clients in the group call).
    // So we need to be careful what we store here.
    audio_forwarder_by_sender_demux_id: HashMap<DemuxId, SingleSsrcRtpForwarder>,
    video_forwarder_by_sender_demux_id: HashMap<DemuxId, Vp8SimulcastRtpForwarder>,
    data_forwarder_by_sender_demux_id: HashMap<DemuxId, SingleSsrcRtpForwarder>,
    allocated_height_by_sender_demux_id: HashMap<DemuxId, VideoHeight>,

    // Used for reliable RTP transmissions point-to-point
    reliable_mrp_stream: mrp::MrpStream<protos::SfuToDevice, protos::DeviceToSfu>,

    // Update with each proto send from server to client
    next_server_to_client_data_rtp_seqnum: rtp::FullSequenceNumber,
}

impl Client {
    fn new(
        pending_client_info: NonParticipantClient,
        initial_target_send_rate: DataRate,
        requested_max_send_rate: DataRate,
        now: Instant,
    ) -> Self {
        Self {
            demux_id: pending_client_info.demux_id,
            user_id: pending_client_info.user_id,
            is_admin: pending_client_info.is_admin,
            region_relation: pending_client_info.region_relation,

            incoming_video0: IncomingVideoState::default(),
            incoming_video1: IncomingVideoState::default(),
            incoming_video2: IncomingVideoState::default(),
            video_rotation: VideoRotation::None,

            incoming_audio_levels: audio::LevelsTracker::default(),
            became_active_speaker: None,

            video_request_proto: None,
            requested_height_by_demux_id: HashMap::new(),
            active_speaker_height: 0,

            target_send_rate: initial_target_send_rate,
            current_generation_min_target_send_rate: initial_target_send_rate,
            old_generation_min_target_send_rate: initial_target_send_rate,
            next_min_target_generation_update_time: now,

            outgoing_queue_drain_rate: DataRate::default(),
            requested_max_send_rate,
            send_rate_allocated: now,

            requested_base_rate: DataRate::default(),
            ideal_send_rate: DataRate::default(),
            allocated_send_rate: DataRate::default(),
            connection_rates: ConnectionRates::default(),

            padding_ssrc: None,

            audio_forwarder_by_sender_demux_id: HashMap::new(),
            video_forwarder_by_sender_demux_id: HashMap::new(),
            data_forwarder_by_sender_demux_id: HashMap::new(),
            allocated_height_by_sender_demux_id: HashMap::new(),

            reliable_mrp_stream: MrpStream::new(MAX_MRP_WINDOW_SIZE),

            next_server_to_client_data_rtp_seqnum: pending_client_info
                .next_server_to_client_data_rtp_seqnum,
        }
    }

    fn parse_vp8_header_and_update_incoming_video_rate_and_resolution(
        &mut self,
        incoming_rtp: &rtp::Packet<&[u8]>,
        now: Instant,
    ) -> Result<Option<VideoHeader>, Error> {
        let incoming_vp8 = if let Some((descriptor, _)) = incoming_rtp.dependency_descriptor {
            VideoHeader::from(descriptor)
        } else {
            VideoHeader::from(
                vp8::ParsedHeader::read(incoming_rtp.payload())
                    .map_err(|_| Error::InvalidVp8Header)?,
            )
        };
        let incoming_layer_id = LayerId::from_ssrc(incoming_rtp.ssrc());
        let size = incoming_rtp.size().as_bytes() as usize;

        let incoming_video = match incoming_layer_id {
            Some(LayerId::Video0) => {
                event!("calling.bandwidth.incoming.video0_bytes", size);
                &mut self.incoming_video0
            }
            Some(LayerId::Video1) => {
                event!("calling.bandwidth.incoming.video1_bytes", size);
                &mut self.incoming_video1
            }
            Some(LayerId::Video2) => {
                event!("calling.bandwidth.incoming.video2_bytes", size);
                &mut self.incoming_video2
            }
            _ => {
                event!("calling.bandwidth.incoming.video_unknown_layer_bytes", size);
                return Err(Error::InvalidRtpLayerId);
            }
        };

        incoming_video.rate_tracker.push_bytes(size, now);

        let old_resolution = incoming_video.original_resolution;
        if let Some(resolution) = incoming_vp8.resolution() {
            incoming_video.original_resolution = Some(resolution);
        }
        let new_resolution = incoming_video.original_resolution;

        // Note: Rotation may be sent in a separate packet than the resolution since it is sent in
        // the last packet for a key frame.
        let old_rotation = self.video_rotation;
        if let Some(rotation) = incoming_rtp.video_rotation {
            self.video_rotation = rotation;
        }

        if old_resolution != new_resolution || old_rotation != self.video_rotation {
            self.incoming_video0.apply_rotation(self.video_rotation);
            self.incoming_video1.apply_rotation(self.video_rotation);
            self.incoming_video2.apply_rotation(self.video_rotation);

            // Clear any higher resolutions.
            // This will be a little inefficient if we get a resolution change for layer 1 before
            // layer 0, but we can't really tell if resolutions between layers match or not.
            match incoming_layer_id {
                Some(LayerId::Video0) => {
                    self.incoming_video1.clear_resolution();
                    self.incoming_video2.clear_resolution();
                }
                Some(LayerId::Video1) => {
                    self.incoming_video2.clear_resolution();
                }
                Some(LayerId::Video2) => {}
                _ => unreachable!("checked above"),
            }
        }
        Ok(Some(incoming_vp8))
    }

    fn forward_audio_rtp(
        &mut self,
        incoming_rtp: &rtp::Packet<&[u8]>,
    ) -> Option<rtp::Packet<Vec<u8>>> {
        let sender_demux_id = DemuxId::from_ssrc(incoming_rtp.ssrc());
        let forwarder = self
            .audio_forwarder_by_sender_demux_id
            .entry(sender_demux_id)
            .or_default();

        let outgoing_ssrc = incoming_rtp.ssrc();
        let outgoing_seqnum = forwarder.forward_rtp(incoming_rtp.seqnum())?;
        let outgoing_timestamp = incoming_rtp.timestamp;
        let outgoing_rtp = incoming_rtp.rewrite(outgoing_ssrc, outgoing_seqnum, outgoing_timestamp);
        Some(outgoing_rtp)
    }

    fn forward_video_rtp(
        &mut self,
        incoming_rtp: &rtp::Packet<&[u8]>,
        incoming_vp8: Option<&VideoHeader>,
    ) -> Option<rtp::Packet<Vec<u8>>> {
        let incoming_vp8 = incoming_vp8?;

        let sender_demux_id = DemuxId::from_ssrc(incoming_rtp.ssrc());
        let forwarder = self
            .video_forwarder_by_sender_demux_id
            .get_mut(&sender_demux_id)?;

        let (outgoing_ssrc, outgoing) = forwarder.forward_vp8_rtp(incoming_rtp, incoming_vp8)?;
        let mut outgoing_rtp = incoming_rtp.rewrite(
            outgoing_ssrc,
            outgoing.seqnum,
            outgoing.timestamp as rtp::TruncatedTimestamp,
        );
        if let Some(frame_number) = outgoing.frame_number {
            if let Some((descriptor, _)) = &mut outgoing_rtp.dependency_descriptor {
                descriptor.truncated_frame_number = Some(frame_number as rtp::TruncatedFrameNumber);
            }
            outgoing_rtp.set_frame_number_in_header(frame_number);
        }
        if let (Some(picture_id), Some(tl0_pic_idx)) = (outgoing.picture_id, outgoing.tl0_pic_idx) {
            vp8::modify_header(
                outgoing_rtp.payload_mut(),
                picture_id as vp8::TruncatedPictureId,
                tl0_pic_idx as vp8::TruncatedTl0PicIdx,
            );
        }
        Some(outgoing_rtp)
    }

    fn forward_data_rtp(
        &mut self,
        incoming_rtp: &rtp::Packet<&[u8]>,
    ) -> Option<rtp::Packet<Vec<u8>>> {
        let sender_demux_id = DemuxId::from_ssrc(incoming_rtp.ssrc());
        let forwarder = self
            .data_forwarder_by_sender_demux_id
            .entry(sender_demux_id)
            .or_default();
        let outgoing_ssrc = incoming_rtp.ssrc();
        let outgoing_seqnum = forwarder.forward_rtp(incoming_rtp.seqnum())?;
        let outgoing_timestamp = incoming_rtp.timestamp;
        let outgoing_rtp = incoming_rtp.rewrite(outgoing_ssrc, outgoing_seqnum, outgoing_timestamp);
        Some(outgoing_rtp)
    }

    fn get_stats(&self) -> ClientStats {
        ClientStats {
            demux_id: self.demux_id,
            user_id: self.user_id.clone(),
            video0_incoming_rate: self.incoming_video0.rate(),
            video1_incoming_rate: self.incoming_video1.rate(),
            video2_incoming_rate: self.incoming_video2.rate(),
            video0_incoming_height: self.incoming_video0.height,
            video1_incoming_height: self.incoming_video1.height,
            video2_incoming_height: self.incoming_video2.height,
            requested_base_rate: self.requested_base_rate,
            min_target_send_rate: self.min_target_send_rate(),
            target_send_rate: self.target_send_rate,
            ideal_send_rate: self.ideal_send_rate,
            allocated_send_rate: self.allocated_send_rate,
            connection_rates: self.connection_rates,
            outgoing_queue_drain_rate: self.outgoing_queue_drain_rate,
            max_requested_height: self.requested_height_by_demux_id.values().max().copied(),
        }
    }

    fn min_target_send_rate(&self) -> DataRate {
        min(
            self.current_generation_min_target_send_rate,
            self.old_generation_min_target_send_rate,
        )
    }
}

#[derive(Default)]
struct IncomingVideoState {
    rate_tracker: DataRateTracker,
    /// The resolution of the video, ignoring rotation.
    original_resolution: Option<PixelSize>,
    /// The height of the video, taking rotation into account.
    height: Option<VideoHeight>,
}

impl IncomingVideoState {
    pub fn rate(&self) -> Option<DataRate> {
        self.rate_tracker.stable_rate()
    }

    fn apply_rotation(&mut self, rotation: VideoRotation) {
        if let Some(resolution) = self.original_resolution {
            let height = match rotation {
                rtp::VideoRotation::None | rtp::VideoRotation::Clockwise180 => resolution.height,
                rtp::VideoRotation::Clockwise90 | rtp::VideoRotation::Clockwise270 => {
                    resolution.width
                }
            };
            self.height = Some(VideoHeight::from(height));
        }
    }

    fn clear_resolution(&mut self) {
        self.original_resolution = None;
        self.height = None;
    }

    fn as_allocatable_layer(&self) -> AllocatableVideoLayer {
        AllocatableVideoLayer {
            incoming_rate: self.rate().unwrap_or_default(),
            incoming_height: self.height.unwrap_or_default(),
        }
    }
}

// This is spatial layers, not temporal layers
#[derive(Clone, Debug)]
struct AllocatableVideoLayer {
    incoming_rate: DataRate,
    incoming_height: VideoHeight,
}

#[derive(Clone, Debug)]
struct AllocatableVideo {
    sender_demux_id: DemuxId,
    // This is spatial layers, not temporal layers
    // lower index == lower resolution
    layers: [AllocatableVideoLayer; 3],
    requested_height: VideoHeight,
    allocated_layer_index: Option<usize>,
    // AKA became active speaker
    interesting: Option<Instant>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AllocatedVideo {
    sender_demux_id: DemuxId,
    layer_index: usize,
    // It is a convenience to include the following fields.
    // They could be derived from AllocatableVideo + layer_index.
    rate: DataRate,
    height: VideoHeight,
}

fn ideal_video_layer_index(video: &AllocatableVideo) -> Option<usize> {
    let requested_height = video.requested_height;
    let has_rate = |layer: &AllocatableVideoLayer| layer.incoming_rate.as_bps() > 0;
    let has_height = |layer: &AllocatableVideoLayer| layer.incoming_height > VideoHeight::from(0);
    let has_height_and_rate = |layer: &AllocatableVideoLayer| has_rate(layer) && has_height(layer);
    let has_enough_height_and_rate = |layer: &AllocatableVideoLayer| {
        layer.incoming_height >= requested_height && has_rate(layer)
    };

    if requested_height == VideoHeight::from(0) {
        // Nothing was requested, so nothing is ideal.
        None
    } else if let Some(first_layer_which_has_enough) =
        video.layers.iter().position(has_enough_height_and_rate)
    {
        // It's possible for several layers to have the ideal height.
        // The ideal layer is the highest layer with the ideal height.
        let ideal_height = video.layers[first_layer_which_has_enough].incoming_height;
        video
            .layers
            .iter()
            .rposition(|layer: &AllocatableVideoLayer| {
                layer.incoming_height == ideal_height && has_rate(layer)
            })
    } else {
        // None of the layers have enough height and rate, so just take the
        // highest layer that has any height and rate.
        video.layers.iter().rposition(has_height_and_rate)
    }
}

fn ideal_send_rate(videos: &[AllocatableVideo], max_requested_send_rate: DataRate) -> DataRate {
    let allocatable: DataRate = videos
        .iter()
        .filter_map(|video| {
            let ideal_layer_index = ideal_video_layer_index(video)?;
            Some(video.layers[ideal_layer_index].incoming_rate)
        })
        .sum();
    min(allocatable, max_requested_send_rate)
}

fn base_video_layer_index(video: &AllocatableVideo) -> Option<usize> {
    if video.requested_height == VideoHeight::from(0) || video.layers[0].incoming_rate.as_bps() == 0
    {
        // Nothing was requested or the base layer doesn't have a rate
        None
    } else {
        Some(0)
    }
}

fn requested_base_rate(videos: &[AllocatableVideo], max_requested_send_rate: DataRate) -> DataRate {
    let allocatable: DataRate = videos
        .iter()
        .filter_map(|video| {
            let base_layer_index = base_video_layer_index(video)?;
            Some(video.layers[base_layer_index].incoming_rate)
        })
        .sum();
    min(allocatable, max_requested_send_rate)
}

fn allocate_send_rate(
    target_send_rate: DataRate,
    min_target_send_rate: DataRate,
    ideal_send_rate: DataRate,
    outgoing_queue_drain_rate: DataRate,
    mut videos: Vec<AllocatableVideo>,
) -> HashMap<DemuxId, AllocatedVideo> {
    // We leave some target send rate unallocated to allow the queue to drain.
    // But if the ideal rate is lower than the target rate, there is room
    // between the ideal rate and the target rate to drain the queue.

    // First use whichever is greater of (minimum target rate minus queue
    // drain rate) and the minimum allocation ratio.
    let allocatable_rate_for_different_layers = max(
        min_target_send_rate.saturating_sub(outgoing_queue_drain_rate),
        min_target_send_rate * TARGET_RATE_MINIMUM_ALLOCATION_RATIO,
    );
    // Now use the lesser of that result and the ideal send rate; layers
    // must be under this bitrate to be allocated, if not currently
    // selected.
    let allocatable_rate_for_different_layers =
        min(allocatable_rate_for_different_layers, ideal_send_rate);

    // Do the same process with the current target rate
    let allocatable_rate_for_existing_layers = max(
        target_send_rate.saturating_sub(outgoing_queue_drain_rate),
        target_send_rate * TARGET_RATE_MINIMUM_ALLOCATION_RATIO,
    );

    // This bitrate will be equal to or greater than the rate for different
    // layers; allowing more bandwidth to be used to keep a currently
    // selected layer than to switch layers, so there's less layer switching
    // as available bandwidth changes.
    let allocatable_rate_for_existing_layers =
        min(allocatable_rate_for_existing_layers, ideal_send_rate);

    let mut allocated_by_sender_demux_id: HashMap<DemuxId, AllocatedVideo> = HashMap::new();
    let mut allocated_rate = DataRate::ZERO;

    // Biggest first and then (for the same size), most recently interesting first
    videos.sort_by_key(|video| std::cmp::Reverse((video.requested_height, video.interesting)));

    // We try to get the lowest layers for each one before trying to get the higher layer for any one.
    // In the future we may want to allow clients to prioritize a video to a degree
    // that it gets all of its layers first.
    for layer_index in 0..=2 {
        trace!("Allocating layer {}", layer_index);
        for video in &videos {
            let mut candidate_layer_index = layer_index;
            let mut layer = &video.layers[candidate_layer_index];

            trace!(
                "Allocating {:?}.{} = ({}, {:?})",
                video.sender_demux_id,
                layer_index,
                layer.incoming_rate.as_kbps(),
                layer.incoming_height
            );
            if layer.incoming_height == VideoHeight::from(0) && layer.incoming_rate.as_bps() == 0 {
                trace!("Skipped layer with nothing coming in.");
                continue;
            }

            if let Some(ideal_layer_index) = ideal_video_layer_index(video) {
                if ideal_layer_index < layer_index {
                    trace!(
                        "Skipped layer that's not requested (ideal layer index: {:?}).",
                        ideal_layer_index
                    );
                    continue;
                }

                for possible_layer_index in layer_index + 1..=ideal_layer_index {
                    let possible_layer = &video.layers[possible_layer_index];
                    if possible_layer.incoming_height != VideoHeight::from(0)
                        && possible_layer.incoming_rate.as_bps() != 0
                        && possible_layer.incoming_rate < layer.incoming_rate
                    {
                        candidate_layer_index = possible_layer_index;
                        layer = possible_layer;
                    }
                }
            } else {
                trace!("Skipped layer that's not requested (ideal layer index: None).");
                continue;
            }

            let layer_rate = layer.incoming_rate;
            let lower_layer_rate = allocated_by_sender_demux_id
                .get(&video.sender_demux_id)
                .map(|allocated| allocated.rate)
                .unwrap_or_default();
            let rate_increase = layer_rate.saturating_sub(lower_layer_rate);
            let increased_allocated_rate = allocated_rate + rate_increase;
            let allocatable_rate = if Some(candidate_layer_index) == video.allocated_layer_index {
                allocatable_rate_for_existing_layers
            } else {
                allocatable_rate_for_different_layers
            };

            if increased_allocated_rate > allocatable_rate {
                trace!(
                    "Skipped layer that's too big ({}/{} allocated and {}={}-{} increase)",
                    allocated_rate.as_kbps(),
                    allocatable_rate.as_kbps(),
                    rate_increase.as_kbps(),
                    layer_rate.as_kbps(),
                    lower_layer_rate.as_kbps()
                );
                continue;
            }

            allocated_by_sender_demux_id.insert(
                video.sender_demux_id,
                AllocatedVideo {
                    sender_demux_id: video.sender_demux_id,
                    layer_index: candidate_layer_index,
                    rate: layer.incoming_rate,
                    height: layer.incoming_height,
                },
            );
            allocated_rate = increased_allocated_rate;
            trace!(
                "Allocated layer.  New allocated_rate: {:?}",
                allocated_rate.as_kbps()
            );
        }
    }
    allocated_by_sender_demux_id
}

// State to allow forwarding one SSRC to one SSRC.
// It's fairly simple, but it must deal with gaps
// in the seqnums and make sure to not reuse expanded seqnums.
// It does this by resetting an offset every time there is
// a gap that is too big to represent.
// This is similar to the VP8 simulcast forwarder
// when it gets a key frame.
#[derive(Default)]
struct SingleSsrcRtpForwarder {
    // When we "reset" due to a big gap (presumably of silence),
    // these are the first seqnums.
    // Knowing these allows us to adjust future packets so they
    // maintain the relative relationship that they did in the
    // unmodified stream of packets.
    // "first" here means "first since latest reset".
    first_incoming: rtp::FullSequenceNumber,
    first_outgoing: rtp::FullSequenceNumber,

    // We have to keep track of the max outgoing seqnums
    // to know what to make the "first" when we reset.
    // (generally, the max + 2).
    max_outgoing: rtp::FullSequenceNumber,
}

impl SingleSsrcRtpForwarder {
    fn forward_rtp(
        &mut self,
        incoming: rtp::FullSequenceNumber,
    ) -> Option<rtp::FullSequenceNumber> {
        const FULL_CYCLE: rtp::FullSequenceNumber =
            (rtp::TruncatedSequenceNumber::MAX as rtp::FullSequenceNumber) + 1;
        const HALF_CYCLE: rtp::FullSequenceNumber = FULL_CYCLE / 2;

        let mut outgoing = self
            .first_outgoing
            .checked_add(incoming.checked_sub(self.first_incoming)?)?;

        if outgoing > (self.max_outgoing + HALF_CYCLE - 1) {
            // The gap is too big.  Reset to a different offset.
            // Make sure to include a gap so the receiver knows there is some loss.
            outgoing = self.max_outgoing.checked_add(2)?;
            self.first_incoming = incoming;
            self.first_outgoing = outgoing;
            self.max_outgoing = outgoing;
        }

        self.max_outgoing = max(self.max_outgoing, outgoing);
        Some(outgoing)
    }
}

// State to allow forwarding a set of N video SSRCs as 1 video SSRC by
// changing the seqnums and VP8 picture IDs and VP8 TL0 Picture Indexes
// to make it appear that it's one stream rather than N.
struct Vp8SimulcastRtpForwarder {
    // The outgoing SSRC.  It never changes.
    outgoing_ssrc: rtp::Ssrc,
    forwarding: Vp8SimulcastRtpForwardingState,
    switching: Vp8SimulcastRtpSwitchingState,
    // We have to keep track of the max outgoing IDs
    // to know what to make the "first" when we switch.
    // (generally, the max + 1).  And we have to retain
    // that outside of the forwarding state below so we
    // retain it across various pause/forward cycles.
    max_outgoing: Vp8RewrittenIds,
}
enum Vp8SimulcastRtpSwitchingState {
    DoNotSwitch,
    SwitchAtNextKeyFrame(rtp::Ssrc),
}

enum Vp8SimulcastRtpForwardingState {
    Paused,
    Forwarding {
        incoming_ssrc: rtp::Ssrc,
        needs_key_frame: bool,

        // When we switch at a key frame, these are the first IDs.
        // Knowing these allows us to adjust future packets so they
        // maintain the relative relationship that they did in the
        // unmodified stream of packets.
        // "first" here means "first since latest switch".
        first_incoming: Vp8RewrittenIds,
        first_outgoing: Vp8RewrittenIds,

        // We have to keep track of the max incoming IDs
        // to be able to expand the IDs from truncated to full.
        // otherwise, rollover would mess up the "max outgoing"
        // below.
        max_incoming: Vp8RewrittenIds,
    },
}

/// The IDs that we rewrite when forwarding VP8.
///
/// This is a convenience for keep track of all 4 together, which is a common
/// thing in Vp8SimulcastRtpForwarder.
#[derive(Debug, Clone, Eq, PartialEq)]
struct Vp8RewrittenIds {
    seqnum: rtp::FullSequenceNumber,
    timestamp: rtp::FullTimestamp,
    picture_id: Option<vp8::FullPictureId>,
    tl0_pic_idx: Option<vp8::FullTl0PicIdx>,
    frame_number: Option<rtp::FullFrameNumber>,
}

impl Default for Vp8RewrittenIds {
    fn default() -> Self {
        Self {
            seqnum: 0,
            timestamp: 0,
            picture_id: Some(0),
            tl0_pic_idx: Some(0),
            frame_number: Some(1),
        }
    }
}

impl Vp8RewrittenIds {
    fn new(
        seqnum: rtp::FullSequenceNumber,
        timestamp: rtp::FullTimestamp,
        picture_id: Option<vp8::FullPictureId>,
        tl0_pic_idx: Option<vp8::FullTl0PicIdx>,
        frame_number: Option<rtp::FullFrameNumber>,
    ) -> Self {
        Self {
            seqnum,
            timestamp,
            picture_id,
            tl0_pic_idx,
            frame_number,
        }
    }

    fn checked_sub(&self, other: &Self) -> Option<Self> {
        let picture_id = if let (Some(lhs), Some(rhs)) = (self.picture_id, other.picture_id) {
            Some(lhs.checked_sub(rhs)?)
        } else {
            None
        };
        let tl0_pic_idx = if let (Some(lhs), Some(rhs)) = (self.tl0_pic_idx, other.tl0_pic_idx) {
            Some(lhs.checked_sub(rhs)?)
        } else {
            None
        };
        let frame_number = if let (Some(lhs), Some(rhs)) = (self.frame_number, other.frame_number) {
            Some(lhs.checked_sub(rhs)?)
        } else {
            None
        };
        Some(Self::new(
            self.seqnum.checked_sub(other.seqnum)?,
            self.timestamp.checked_sub(other.timestamp)?,
            picture_id,
            tl0_pic_idx,
            frame_number,
        ))
    }

    fn checked_add(&self, other: &Self) -> Option<Self> {
        let picture_id = if let (Some(lhs), Some(rhs)) = (self.picture_id, other.picture_id) {
            Some(lhs.checked_add(rhs)?)
        } else {
            None
        };
        let tl0_pic_idx = if let (Some(lhs), Some(rhs)) = (self.tl0_pic_idx, other.tl0_pic_idx) {
            Some(lhs.checked_add(rhs)?)
        } else {
            None
        };
        let frame_number = if let (Some(lhs), Some(rhs)) = (self.frame_number, other.frame_number) {
            Some(lhs.checked_add(rhs)?)
        } else {
            None
        };
        Some(Self::new(
            self.seqnum.checked_add(other.seqnum)?,
            self.timestamp.checked_add(other.timestamp)?,
            picture_id,
            tl0_pic_idx,
            frame_number,
        ))
    }

    fn max(&self, other: &Self) -> Self {
        Self::new(
            max(self.seqnum, other.seqnum),
            max(self.timestamp, other.timestamp),
            max(self.picture_id, other.picture_id),
            max(self.tl0_pic_idx, other.tl0_pic_idx),
            max(self.frame_number, other.frame_number),
        )
    }
}

impl Vp8SimulcastRtpForwarder {
    fn new(outgoing_ssrc: rtp::Ssrc) -> Self {
        Self {
            outgoing_ssrc,
            forwarding: Vp8SimulcastRtpForwardingState::Paused,
            switching: Vp8SimulcastRtpSwitchingState::DoNotSwitch,
            max_outgoing: Vp8RewrittenIds::default(),
        }
    }

    fn switching_ssrc(&self) -> Option<rtp::Ssrc> {
        if let Vp8SimulcastRtpSwitchingState::SwitchAtNextKeyFrame(switch_ssrc) = self.switching {
            Some(switch_ssrc)
        } else {
            None
        }
    }

    fn forwarding_ssrc(&self) -> Option<rtp::Ssrc> {
        if let Vp8SimulcastRtpForwardingState::Forwarding {
            incoming_ssrc: forward_ssrc,
            ..
        } = self.forwarding
        {
            Some(forward_ssrc)
        } else {
            None
        }
    }

    fn needs_key_frame(&self) -> Option<rtp::Ssrc> {
        if let Vp8SimulcastRtpSwitchingState::SwitchAtNextKeyFrame(switching_ssrc) = self.switching
        {
            Some(switching_ssrc)
        } else if let Vp8SimulcastRtpForwardingState::Forwarding {
            incoming_ssrc: forwarding_ssrc,
            needs_key_frame: true,
            ..
        } = self.forwarding
        {
            Some(forwarding_ssrc)
        } else {
            None
        }
    }

    // If the SSRC is set to None, don't forward anything.
    fn set_desired_ssrc(&mut self, desired_incoming_ssrc: Option<rtp::Ssrc>) {
        if let Some(desired_incoming_ssrc) = desired_incoming_ssrc {
            if self.forwarding_ssrc() != Some(desired_incoming_ssrc)
                && self.switching_ssrc() != Some(desired_incoming_ssrc)
            {
                trace!(
                    "Begin forwarding from SSRC {} to SSRC {} once we receive a key frame.",
                    desired_incoming_ssrc,
                    self.outgoing_ssrc
                );
                match self.forwarding_ssrc() {
                    Some(ssrc) if desired_incoming_ssrc > ssrc => {
                        event!("calling.forwarding.layer_switch.higher")
                    }
                    Some(_) => event!("calling.forwarding.layer_switch.lower"),
                    None => event!("calling.forwarding.layer_switch.start"),
                }
                self.switching =
                    Vp8SimulcastRtpSwitchingState::SwitchAtNextKeyFrame(desired_incoming_ssrc);
            } else if self.forwarding_ssrc() == Some(desired_incoming_ssrc)
                && self.switching_ssrc().is_some()
            {
                let switching_ssrc = self.switching_ssrc().expect("switching_ssrc was not None");
                trace!(
                    "switch back to SSRC {} to SSRC {} while waiting for key frame for {}.",
                    desired_incoming_ssrc,
                    self.outgoing_ssrc,
                    switching_ssrc
                );
                if desired_incoming_ssrc > switching_ssrc {
                    event!("calling.forwarding.layer_switch.higher_while_waiting");
                } else {
                    event!("calling.forwarding.layer_switch.lower_while_waiting");
                }
            }
        } else {
            if self.forwarding_ssrc().is_some() {
                trace!("Stop forwarding to SSRC {}", self.outgoing_ssrc);
                event!("calling.forwarding.layer_switch.stop");
            }

            self.forwarding = Vp8SimulcastRtpForwardingState::Paused;
            self.switching = Vp8SimulcastRtpSwitchingState::DoNotSwitch;
        }
    }

    // Set this when the receiving clients sends a key frame request for the sender.
    fn set_needs_key_frame(&mut self) {
        // Don't pause because packets arriving out of order would not get delivered
        // and we'd perhaps need to request a new key frame yet again.
        // Plus, pausing messes up the congestion controller.
        if let Vp8SimulcastRtpForwardingState::Forwarding {
            needs_key_frame, ..
        } = &mut self.forwarding
        {
            *needs_key_frame = true;
        }
    }

    // Selects a new seqnum, VP8 Picture ID, and VP8 Tl0PicIdx.  If None is returned, that means
    // don't forward the packet.
    fn forward_vp8_rtp(
        &mut self,
        incoming_rtp: &rtp::Packet<&[u8]>,
        incoming_vp8: &VideoHeader,
    ) -> Option<(rtp::Ssrc, Vp8RewrittenIds)> {
        // Both IDs are None when a dependency descriptor is used, otherwise they're both Some.
        match (
            incoming_vp8,
            incoming_vp8.picture_id(),
            incoming_vp8.tl0_pic_idx(),
        ) {
            (VideoHeader::VP8(_), None, _)
            | (VideoHeader::VP8(_), _, None)
            | (VideoHeader::DependencyDescriptor(_), Some(_), _)
            | (VideoHeader::DependencyDescriptor(_), _, Some(_)) => {
                return None;
            }
            _ => {}
        }

        if self.switching_ssrc() == Some(incoming_rtp.ssrc()) && incoming_vp8.is_key_frame() {
            trace!(
                "Begin forwarding from SSRC {} to SSRC {} because we have a key frame.",
                incoming_rtp.ssrc(),
                self.outgoing_ssrc
            );

            let first_incoming = Vp8RewrittenIds::new(
                incoming_rtp.seqnum(),
                // These are OK to expand without one of the expand_X functions because
                // they are only used as a base for future values.
                // In other words, we are only tracking the ROC since the switching point,
                // and that is now, so the ROC is 0.
                incoming_rtp.timestamp as rtp::FullTimestamp,
                incoming_vp8.picture_id().map(|id| id as vp8::FullPictureId),
                incoming_vp8
                    .tl0_pic_idx()
                    .map(|idx| idx as vp8::FullTl0PicIdx),
                incoming_vp8
                    .truncated_frame_number()
                    .map(|num| num as rtp::FullFrameNumber),
            );
            // We make two simplifying assumptions here:
            // 1. The first packet we received is the first packet of the key frame.
            // If this is false (due to reordering), the receiving client will have to
            // ask for a new key frame.  That should hopefully be infrequent.
            // 2. The last packet we forwarded (of the previous layer) is the last packet we'd ever want to forward.
            // If this is false, the last frame of the previous layer will be dropped by the receiving client.
            // Which hopefully will not be noticeable.
            // These assumptions allow us to have no gap between the last seqnum before the switch
            // and the first seqnum/picture_id after the switch and doesn't require any fancy logic or queuing.
            // Ok, there is a gap of 1 seqnum to signify to the encoder that the
            // previous frame was (probably) incomplete.  That's why there's a 2 for the seqnum.
            let first_outgoing = self.max_outgoing.checked_add(&Vp8RewrittenIds::new(
                2,
                1,
                Some(1),
                Some(1),
                Some(1),
            ))?;

            self.forwarding = Vp8SimulcastRtpForwardingState::Forwarding {
                incoming_ssrc: incoming_rtp.ssrc(),
                first_incoming: first_incoming.clone(),
                first_outgoing: first_outgoing.clone(),
                max_incoming: first_incoming,
                needs_key_frame: false,
            };
            self.switching = Vp8SimulcastRtpSwitchingState::DoNotSwitch;
            self.max_outgoing = first_outgoing;
        }

        if let Vp8SimulcastRtpForwardingState::Forwarding {
            incoming_ssrc,
            first_incoming,
            first_outgoing,
            max_incoming,
            needs_key_frame,
            ..
        } = &mut self.forwarding
        {
            if *incoming_ssrc == incoming_rtp.ssrc() {
                let expanded_picture_id = if let (Some(incoming), Some(max_incoming)) =
                    (incoming_vp8.picture_id(), max_incoming.picture_id.as_mut())
                {
                    Some(vp8::expand_picture_id(incoming, max_incoming))
                } else {
                    None
                };
                let expanded_tl0_pic_idx = if let (Some(incoming), Some(max_incoming)) = (
                    incoming_vp8.tl0_pic_idx(),
                    max_incoming.tl0_pic_idx.as_mut(),
                ) {
                    Some(vp8::expand_tl0_pic_idx(incoming, max_incoming))
                } else {
                    None
                };
                let expanded_frame_number = if let (Some(incoming), Some(max_incoming)) = (
                    incoming_vp8.truncated_frame_number(),
                    max_incoming.frame_number.as_mut(),
                ) {
                    Some(rtp::expand_frame_number(incoming, max_incoming))
                } else {
                    None
                };

                let incoming = Vp8RewrittenIds::new(
                    incoming_rtp.seqnum(),
                    rtp::expand_timestamp(incoming_rtp.timestamp, &mut max_incoming.timestamp),
                    expanded_picture_id,
                    expanded_tl0_pic_idx,
                    expanded_frame_number,
                );
                // If the sub fails, it's because the incoming packet predates the switch (before the key frame)
                let outgoing =
                    first_outgoing.checked_add(&incoming.checked_sub(first_incoming)?)?;
                self.max_outgoing = self.max_outgoing.max(&outgoing);

                if incoming_vp8.is_key_frame() {
                    *needs_key_frame = false;
                }
                trace!(
                    "Forward packet from SSRC {} to SSRC {} while rewriting IDs from {:?} to {:?}",
                    incoming_rtp.ssrc(),
                    self.outgoing_ssrc,
                    incoming,
                    outgoing
                );
                Some((self.outgoing_ssrc, outgoing))
            } else {
                // Forwarding a different SSRC
                None
            }
        } else {
            // Not forwarding at all
            None
        }
    }
}

pub struct CallStats {
    pub loggable_call_id: LoggableCallId,
    pub clients: Vec<ClientStats>,
}

pub struct ClientStats {
    pub demux_id: DemuxId,
    pub user_id: UserId,
    pub video0_incoming_rate: Option<DataRate>,
    pub video1_incoming_rate: Option<DataRate>,
    pub video2_incoming_rate: Option<DataRate>,
    pub video0_incoming_height: Option<VideoHeight>,
    pub video1_incoming_height: Option<VideoHeight>,
    pub video2_incoming_height: Option<VideoHeight>,
    pub min_target_send_rate: DataRate,
    pub target_send_rate: DataRate,
    pub requested_base_rate: DataRate,
    pub ideal_send_rate: DataRate,
    pub allocated_send_rate: DataRate,
    pub connection_rates: ConnectionRates,
    pub outgoing_queue_drain_rate: DataRate,
    pub max_requested_height: Option<VideoHeight>,
}

#[cfg(test)]
mod loggable_call_id_tests {
    use hex_literal::hex;

    use super::*;

    #[test]
    fn display_call_id_16_long() {
        let bytes = hex!("000102030405060708090a0b0c0d0e0f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("000102", format!("{}", call_id));
        assert_eq!("000102", call_id.to_string());
    }

    #[test]
    fn display_call_id_64_long() {
        let bytes = hex!("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f222122232425262728292a2b2c2d2e2f033132333435363738393a3b3c3d3e3f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("000102", format!("{}", call_id));
        assert_eq!("000102", call_id.to_string());
    }

    #[test]
    fn display_empty_call_id() {
        let bytes = [];
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("<EMPTY>", format!("{}", call_id));
    }

    #[test]
    fn display_short_call_id_4_bytes() {
        let bytes = hex!("0c0d0e0f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("0c0d0e", format!("{}", call_id));
    }

    #[test]
    fn display_short_call_id_3_bytes() {
        let bytes = hex!("0d0e0f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("0d0e0f", format!("{}", call_id));
    }

    #[test]
    fn display_short_call_id_2_bytes() {
        let bytes = hex!("0e0f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("0e0f", format!("{}", call_id));
    }

    #[test]
    fn display_short_call_id_1_byte() {
        let bytes = hex!("0f");
        let call_id: LoggableCallId = bytes[..].into();

        assert_eq!("0f", format!("{}", call_id));
    }
}

#[cfg(test)]
mod call_tests {
    use super::*;
    use calling_common::PixelSize;

    #[test]
    fn test_forward_audio() {
        let full = (rtp::TruncatedSequenceNumber::MAX as rtp::FullSequenceNumber) + 1;
        let half = full / 2;
        let expand = |full: rtp::FullSequenceNumber, max: &mut rtp::FullSequenceNumber| {
            rtp::expand_seqnum(full as rtp::TruncatedSequenceNumber, max)
        };

        let mut forwarder = SingleSsrcRtpForwarder::default();
        let mut receiver_max = 0;
        let mut used_seqnums = std::collections::HashSet::new();
        for range in [1..half, full..(2 * full)] {
            for seqnum_in in range {
                let seqnum_out = forwarder.forward_rtp(seqnum_in).unwrap();
                // Make sure we never reuse a seqnum
                let not_reused = used_seqnums.insert(seqnum_out);
                assert!(not_reused, "Reused seqnum {}", seqnum_out);
                // Make sure the receiver can always keep track of the ROC
                assert_eq!(seqnum_out, expand(seqnum_out, &mut receiver_max));
            }
        }
        // Don't try to send anything from before the gap.
        for seqnum_in in 1..half {
            assert_eq!(None, forwarder.forward_rtp(seqnum_in));
        }

        // Do try and send things out of order
        let seqnum_out = forwarder.forward_rtp(2 * full + 5).unwrap();
        assert_eq!(Some(seqnum_out - 1), forwarder.forward_rtp(2 * full + 4));
        assert_eq!(Some(seqnum_out - 2), forwarder.forward_rtp(2 * full + 3));
        assert_eq!(Some(seqnum_out - 3), forwarder.forward_rtp(2 * full + 2));
    }

    #[test]
    fn test_forward_vp8() {
        // This is a convenience struct to make the tests more readable.
        #[derive(Clone)]
        struct Incoming {
            ssrc: u32,
            index: u32,
            rtp: rtp::Packet<Vec<u8>>,
            dependency_descriptor: rtp::DependencyDescriptor,
        }

        impl Incoming {
            fn start_with_key_frame(ssrc: u32, width: u16, height: u16) -> Self {
                let index = 1;
                let is_key_frame = true;
                Self::new(ssrc, index, is_key_frame, Some(PixelSize { width, height }))
            }

            fn increment_without_key_frame(&self) -> Self {
                let is_key_frame = false;
                let resolution = None;
                Self::new(self.rtp.ssrc(), self.index + 1, is_key_frame, resolution)
            }

            fn increment_with_key_frame(&self) -> Self {
                let is_key_frame = true;
                Self::new(
                    self.rtp.ssrc(),
                    self.index + 1,
                    is_key_frame,
                    self.dependency_descriptor.resolution,
                )
            }

            fn new(
                ssrc: u32,
                index: u32,
                is_key_frame: bool,
                resolution: Option<PixelSize>,
            ) -> Self {
                let pt = 108;
                let seqnum = ((ssrc * 10000) + index) as u64;
                let timestamp = (ssrc * 100000) + (index * 30000); // 30 fps at 90khz clock
                Self {
                    ssrc,
                    index,
                    rtp: rtp::Packet::with_empty_tag(pt, seqnum, timestamp, ssrc, None, None, &[]),
                    dependency_descriptor: rtp::DependencyDescriptor {
                        truncated_frame_number: Some(((1000 * ssrc) + index) as u16),
                        is_key_frame,
                        resolution,
                    },
                }
            }

            fn skip_to(
                &self,
                seqnum: rtp::FullSequenceNumber,
                timestamp: rtp::TruncatedTimestamp,
                truncated_frame_number: rtp::TruncatedFrameNumber,
            ) -> Self {
                let mut rtp = self.rtp.clone();
                rtp.set_seqnum_in_header(seqnum);
                rtp.set_timestamp_in_header(timestamp);
                Self {
                    rtp,
                    dependency_descriptor: rtp::DependencyDescriptor {
                        truncated_frame_number: Some(truncated_frame_number),
                        ..self.dependency_descriptor
                    },
                    ..self.clone()
                }
            }

            fn forward(
                &self,
                forwarder: &mut Vp8SimulcastRtpForwarder,
            ) -> Option<(rtp::Ssrc, Vp8RewrittenIds)> {
                forwarder.forward_vp8_rtp(&self.rtp.borrow(), &self.dependency_descriptor.into())
            }
        }

        let outgoing_ssrc = 99;

        // This is a convenience function to make the test more readable.
        let outgoing = |seqnum: rtp::FullSequenceNumber,
                        timestamp: rtp::FullTimestamp,
                        frame_number: rtp::FullFrameNumber|
         -> Option<(rtp::Ssrc, Vp8RewrittenIds)> {
            Some((
                outgoing_ssrc,
                Vp8RewrittenIds {
                    seqnum,
                    timestamp,
                    picture_id: None,
                    tl0_pic_idx: None,
                    frame_number: Some(frame_number),
                },
            ))
        };

        let mut forwarder = Vp8SimulcastRtpForwarder::new(outgoing_ssrc);

        // Nothing desired yet.  Don't send key frame requests and don't forward packets.
        let layer0 = Incoming::start_with_key_frame(0, 320, 180);
        assert_eq!(None, forwarder.needs_key_frame());
        assert_eq!(None, layer0.forward(&mut forwarder));

        // Layer 0 desired.  Send key frame requests and forward a key frame and subsequent packets.
        forwarder.set_desired_ssrc(Some(layer0.ssrc));
        assert_eq!(Some(layer0.ssrc), forwarder.needs_key_frame());
        assert_eq!(outgoing(2, 1, 2), layer0.forward(&mut forwarder));
        let layer0 = layer0.increment_without_key_frame();
        assert_eq!(outgoing(3, 30001, 3), layer0.forward(&mut forwarder));

        let layer1 = Incoming::start_with_key_frame(1, 640, 360);
        let layer1_original = layer1.clone();
        let layer2 = Incoming::start_with_key_frame(2, 1280, 720);
        // But don't forward packets from other layers
        assert_eq!(None, layer1.forward(&mut forwarder));
        assert_eq!(None, layer2.forward(&mut forwarder));

        // If nothing is desired again, don't send key frame requests and don't forward packets.
        forwarder.set_desired_ssrc(None);
        assert_eq!(None, forwarder.needs_key_frame());
        assert_eq!(
            None,
            layer0.increment_with_key_frame().forward(&mut forwarder)
        );
        assert_eq!(None, layer0.forward(&mut forwarder));

        // Once desired again, forward again once we have key frames
        forwarder.set_desired_ssrc(Some(layer0.ssrc));
        assert_eq!(Some(layer0.ssrc), forwarder.needs_key_frame());
        let layer0 = layer0.increment_without_key_frame();
        assert_eq!(None, layer0.forward(&mut forwarder));
        let layer0 = layer0.increment_with_key_frame();
        // There is a gap in the sequence number on purpose to indicate the last frame of the previous
        // layer wasn't finished.
        assert_eq!(outgoing(5, 30002, 4), layer0.forward(&mut forwarder));

        // We no longer need a key frame
        assert_eq!(None, forwarder.needs_key_frame());
        let layer0 = layer0.increment_without_key_frame();
        assert_eq!(outgoing(6, 60002, 5), layer0.forward(&mut forwarder));

        // Request a switch to a higher layer
        // Continue to forward the existing layer until a key frame comes.
        forwarder.set_desired_ssrc(Some(layer1.ssrc));
        let layer0 = layer0.increment_without_key_frame();
        assert_eq!(outgoing(7, 90002, 6), layer0.forward(&mut forwarder));
        let layer1 = layer1.increment_without_key_frame();
        assert_eq!(None, layer1.forward(&mut forwarder));
        let layer2 = layer2.increment_without_key_frame();
        assert_eq!(None, layer2.forward(&mut forwarder));

        // Once we get a key frame, switch
        let layer1 = layer1.increment_with_key_frame();
        assert_eq!(outgoing(9, 90003, 7), layer1.forward(&mut forwarder));
        assert_eq!(None, forwarder.needs_key_frame());
        let layer0 = layer0.increment_with_key_frame();
        assert_eq!(None, layer0.forward(&mut forwarder));
        let layer2 = layer2.increment_with_key_frame();
        assert_eq!(None, layer2.forward(&mut forwarder));

        // Don't forward old packets from the new layer.
        // Such a packet would be prior to the key frame, which means it
        // can't be decoded.
        assert_eq!(None, layer1_original.forward(&mut forwarder));

        // Request another layer
        // Continue to forward the existing layer until a key frame comes.
        forwarder.set_desired_ssrc(Some(layer2.ssrc));
        assert_eq!(Some(layer2.ssrc), forwarder.needs_key_frame());
        let layer0 = layer0.increment_with_key_frame();
        assert_eq!(None, layer0.forward(&mut forwarder));
        let layer1 = layer1.increment_without_key_frame();
        assert_eq!(outgoing(10, 120003, 8), layer1.forward(&mut forwarder));
        let layer2 = layer2.increment_without_key_frame();
        assert_eq!(None, layer2.forward(&mut forwarder));

        let layer2 = layer2.increment_with_key_frame();
        assert_eq!(outgoing(12, 120004, 9), layer2.forward(&mut forwarder));
        let layer0 = layer0.increment_with_key_frame();
        assert_eq!(None, layer0.forward(&mut forwarder));
        let layer1 = layer1.increment_with_key_frame();
        assert_eq!(None, layer1.forward(&mut forwarder));

        // Now go back to layer0
        forwarder.set_desired_ssrc(Some(layer0.ssrc));
        assert_eq!(Some(layer0.ssrc), forwarder.needs_key_frame());
        let layer0 = layer0.increment_without_key_frame();
        assert_eq!(None, layer0.forward(&mut forwarder));
        let layer1 = layer1.increment_with_key_frame();
        assert_eq!(None, layer1.forward(&mut forwarder));
        let layer2 = layer2.increment_without_key_frame();
        assert_eq!(outgoing(13, 150004, 10), layer2.forward(&mut forwarder));

        let layer0 = layer0.increment_with_key_frame();
        assert_eq!(outgoing(15, 150005, 11), layer0.forward(&mut forwarder));
        let layer1 = layer1.increment_with_key_frame();
        assert_eq!(None, layer1.forward(&mut forwarder));
        let layer2 = layer2.increment_with_key_frame();
        assert_eq!(None, layer2.forward(&mut forwarder));

        // If something goes wrong with this layer, keep forwarding it
        // but request a key frame until we get one.
        forwarder.set_needs_key_frame();
        assert_eq!(Some(layer0.ssrc), forwarder.needs_key_frame());
        let layer0 = layer0.increment_with_key_frame();
        assert_eq!(outgoing(16, 180005, 12), layer0.forward(&mut forwarder));
        assert_eq!(None, forwarder.needs_key_frame());

        // Unless we desire a higher layer, then request that instead.
        forwarder.set_needs_key_frame();
        assert_eq!(Some(layer0.ssrc), forwarder.needs_key_frame());
        forwarder.set_desired_ssrc(Some(layer1.ssrc));
        assert_eq!(Some(layer1.ssrc), forwarder.needs_key_frame());

        // Also forward packets even if they're out of order
        let layer0a = layer0.increment_without_key_frame();
        let layer0b = layer0a.increment_without_key_frame();
        assert_eq!(outgoing(18, 240005, 14), layer0b.forward(&mut forwarder));
        assert_eq!(outgoing(17, 210005, 13), layer0a.forward(&mut forwarder));

        // And deal with roll over properly (pretend there's a long gap of no forwarding)
        forwarder.set_desired_ssrc(None);
        assert_eq!(None, forwarder.needs_key_frame());
        assert_eq!(None, layer0.forward(&mut forwarder));
        assert_eq!(None, layer1.forward(&mut forwarder));
        assert_eq!(None, layer2.forward(&mut forwarder));

        forwarder.set_desired_ssrc(Some(layer0.ssrc));
        let max_seqnum = u16::MAX as u64;
        let max_timestamp = u32::MAX;
        let max_frame_number = u16::MAX;
        let layer0_before_rollover =
            layer0
                .increment_with_key_frame()
                .skip_to(max_seqnum, max_timestamp, max_frame_number);
        let layer0_after_rollover = layer0.skip_to(max_seqnum + 1, 0, 0);
        assert_eq!(
            outgoing(20, 240006, 15),
            layer0_before_rollover.forward(&mut forwarder)
        );
        assert_eq!(
            outgoing(21, 240007, 16),
            layer0_after_rollover.forward(&mut forwarder)
        );
    }

    #[test]
    fn test_allocate_send_rate() {
        // Convenience methods to make test more readable
        fn layer(incoming_rate_kbps: u64, incoming_height: VideoHeight) -> AllocatableVideoLayer {
            AllocatableVideoLayer {
                incoming_rate: DataRate::from_kbps(incoming_rate_kbps),
                incoming_height,
            }
        }

        fn video(
            sender_demux_id: DemuxId,
            layers: [&AllocatableVideoLayer; 3],
        ) -> AllocatableVideo {
            AllocatableVideo {
                sender_demux_id,
                layers: [layers[0].clone(), layers[1].clone(), layers[2].clone()],
                requested_height: VideoHeight::from(0),
                allocated_layer_index: None,
                interesting: None,
            }
        }

        fn request(requested_height: VideoHeight, video: &AllocatableVideo) -> AllocatableVideo {
            let mut video: AllocatableVideo = video.clone();
            video.requested_height = requested_height;
            video
        }

        fn request_with_layer(
            requested_height: VideoHeight,
            video: &AllocatableVideo,
            allocated_layer: usize,
        ) -> AllocatableVideo {
            let mut video: AllocatableVideo = video.clone();
            video.requested_height = requested_height;
            video.allocated_layer_index = Some(allocated_layer);
            video
        }

        fn interesting(secs_ago: u64, video: &AllocatableVideo) -> AllocatableVideo {
            let mut video: AllocatableVideo = video.clone();
            video.interesting = Some(Instant::now() - Duration::from_secs(secs_ago));
            video
        }

        fn allocate(
            target_send_rate_kbps: u64,
            min_target_send_rate_kbps: u64,
            outgoing_queue_drain_rate_kbps: u64,
            videos: &[&AllocatableVideo],
            max_requested_send_rate_kbps: u64,
        ) -> (u64, Vec<(u32, usize, u64)>) {
            let videos: Vec<AllocatableVideo> = videos.iter().copied().cloned().collect();
            let target_send_rate = DataRate::from_kbps(target_send_rate_kbps);
            let min_target_send_rate = DataRate::from_kbps(min_target_send_rate_kbps);
            let outgoing_queue_drain_rate = DataRate::from_kbps(outgoing_queue_drain_rate_kbps);
            let max_requested_send_rate = DataRate::from_kbps(max_requested_send_rate_kbps);
            let ideal_send_rate = ideal_send_rate(&videos, max_requested_send_rate);
            let mut allocated: Vec<_> = allocate_send_rate(
                target_send_rate,
                min_target_send_rate,
                ideal_send_rate,
                outgoing_queue_drain_rate,
                videos,
            )
            .iter()
            .map(|(demux_id, allocated)| {
                (
                    u32::from(*demux_id),
                    allocated.layer_index,
                    allocated.rate.as_kbps(),
                )
            })
            .collect();
            allocated.sort_unstable();
            (ideal_send_rate.as_kbps(), allocated)
        }

        let nothing = layer(0, VideoHeight::from(0));
        let layer0 = layer(200, VideoHeight::from(180));
        let layer1 = layer(800, VideoHeight::from(360));
        let layer2 = layer(2000, VideoHeight::from(720));
        let dropped = layer(0, VideoHeight::from(1080));
        let video0 = video(DemuxId::from_const(0x00), [&nothing, &nothing, &nothing]);
        let video1 = video(DemuxId::from_const(0x10), [&layer0, &dropped, &nothing]);
        let video2 = video(DemuxId::from_const(0x20), [&layer0, &layer1, &nothing]);
        let video3 = video(DemuxId::from_const(0x30), [&layer0, &layer1, &layer2]);
        let video4 = video(DemuxId::from_const(0x40), [&layer0, &layer1, &layer2]);
        let no_max = 100000;

        // Can't send and nothing to receive
        assert_eq!((0, vec![]), allocate(0, 0, 0, &[], no_max));
        assert_eq!((0, vec![]), allocate(0, 0, 200, &[], no_max));

        // Can send but nothing to receive
        assert_eq!((0, vec![]), allocate(1000, 1000, 0, &[], no_max));
        assert_eq!((0, vec![]), allocate(1000, 1000, 200, &[], no_max));
        assert_eq!((0, vec![]), allocate(1000, 1000, 0, &[&video0], no_max));
        assert_eq!((0, vec![]), allocate(1000, 1000, 200, &[&video0], no_max));

        // Can send and receive but nothing requested.
        assert_eq!((0, vec![]), allocate(1000, 1000, 0, &[&video1], no_max));
        assert_eq!((0, vec![]), allocate(1000, 1000, 200, &[&video1], no_max));

        // Can receive and requested, but nothing to send.
        assert_eq!(
            (200, vec![]),
            allocate(
                0,
                0,
                0,
                &[&request(VideoHeight::from(1080), &video1)],
                no_max
            )
        );
        assert_eq!(
            (200, vec![]),
            allocate(
                0,
                0,
                200,
                &[&request(VideoHeight::from(1080), &video1)],
                no_max
            )
        );

        // Finally can send, receive, and have requested
        assert_eq!(
            (200, vec![(0x10, 0, 200)]),
            allocate(
                1000,
                1000,
                0,
                &[&request(VideoHeight::from(1080), &video1)],
                no_max
            )
        );
        assert_eq!(
            (200, vec![(0x10, 0, 200)]),
            allocate(
                1000,
                1000,
                200,
                &[&request(VideoHeight::from(1080), &video1)],
                no_max
            )
        );

        // Verify we fill lower layers first, starting with highest resolution requested.
        assert_eq!(
            (3000, vec![]),
            allocate(
                200,
                200,
                200,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x30, 0, 200)]),
            allocate(
                400,
                400,
                200,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                600,
                400,
                200,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request_with_layer(VideoHeight::from(360), &video2, 0),
                    &request_with_layer(VideoHeight::from(720), &video3, 0)
                ],
                no_max
            )
        );

        assert_eq!(
            (3000, vec![(0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                400,
                400,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                667,
                600,
                100,
                &[
                    &request_with_layer(VideoHeight::from(180), &video1, 0),
                    &request_with_layer(VideoHeight::from(360), &video2, 0),
                    &request_with_layer(VideoHeight::from(720), &video3, 0)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                600,
                600,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 1, 800)]),
            allocate(
                1334,
                1200,
                1000,
                &[
                    &request_with_layer(VideoHeight::from(180), &video1, 0),
                    &request_with_layer(VideoHeight::from(360), &video2, 0),
                    &request_with_layer(VideoHeight::from(720), &video3, 1)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 1, 800)]),
            allocate(
                1200,
                1200,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 1, 800), (0x30, 1, 800)]),
            allocate(
                1800,
                1800,
                0,
                &[
                    &request_with_layer(VideoHeight::from(180), &video1, 0),
                    &request_with_layer(VideoHeight::from(360), &video2, 1),
                    &request_with_layer(VideoHeight::from(720), &video3, 1)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 1, 800), (0x30, 1, 800)]),
            allocate(
                2000,
                2000,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 1, 800), (0x30, 2, 2000)]),
            allocate(
                4000,
                3000,
                1000,
                &[
                    &request_with_layer(VideoHeight::from(180), &video1, 0),
                    &request_with_layer(VideoHeight::from(360), &video2, 1),
                    &request_with_layer(VideoHeight::from(720), &video3, 2)
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 1, 800), (0x30, 2, 2000)]),
            allocate(
                3000,
                3000,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                no_max
            )
        );

        // We ignore higher bitrates available if we request a max
        assert_eq!(
            (1200, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 1, 800)]),
            allocate(
                3000,
                3000,
                0,
                &[
                    &request(VideoHeight::from(180), &video1),
                    &request(VideoHeight::from(360), &video2),
                    &request(VideoHeight::from(720), &video3)
                ],
                1200
            )
        );

        // If we have extra, nothing changes.
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 1, 800), (0x30, 2, 2000)]),
            allocate(
                5000,
                5000,
                0,
                &[
                    &request(VideoHeight::from(1080), &video1),
                    &request(VideoHeight::from(1081), &video2),
                    &request(VideoHeight::from(1082), &video3)
                ],
                no_max
            )
        );

        // If we request less, things drop off, including the ideal rate
        assert_eq!(
            (600, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                5000,
                5000,
                0,
                &[
                    &request(VideoHeight::from(1), &video1),
                    &request(VideoHeight::from(2), &video2),
                    &request(VideoHeight::from(3), &video3)
                ],
                no_max
            )
        );

        // If all requests are the same, the interest time determines fill order
        assert_eq!(
            (3000, vec![(0x10, 0, 200)]),
            allocate(
                200,
                200,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(1, &video1)),
                    &request(VideoHeight::from(1080), &interesting(2, &video2)),
                    &request(VideoHeight::from(1080), &interesting(3, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (3000, vec![(0x10, 0, 200), (0x20, 0, 200)]),
            allocate(
                400,
                400,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(1, &video1)),
                    &request(VideoHeight::from(1080), &interesting(2, &video2)),
                    &request(VideoHeight::from(1080), &interesting(3, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (5000, vec![(0x10, 0, 200), (0x20, 0, 200), (0x40, 0, 200)]),
            allocate(
                600,
                600,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(1, &video1)),
                    &request(VideoHeight::from(1080), &interesting(2, &video2)),
                    &request(VideoHeight::from(1080), &interesting(3, &video4)),
                    &request(VideoHeight::from(1080), &interesting(4, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (
                5000,
                vec![
                    (0x10, 0, 200),
                    (0x20, 0, 200),
                    (0x30, 0, 200),
                    (0x40, 0, 200)
                ]
            ),
            allocate(
                800,
                800,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(1, &video1)),
                    &request(VideoHeight::from(1080), &interesting(2, &video2)),
                    &request(VideoHeight::from(1080), &interesting(3, &video4)),
                    &request(VideoHeight::from(1080), &interesting(4, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (4000, vec![(0x30, 0, 200), (0x40, 1, 800)]),
            allocate(
                1000,
                1000,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(3, &video4)),
                    &request(VideoHeight::from(1080), &interesting(4, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (4000, vec![(0x30, 1, 800), (0x40, 2, 2000)]),
            allocate(
                2800,
                2800,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(3, &video4)),
                    &request(VideoHeight::from(1080), &interesting(4, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (4000, vec![(0x30, 2, 2000), (0x40, 2, 2000)]),
            allocate(
                10000,
                10000,
                0,
                &[
                    &request(VideoHeight::from(1080), &interesting(3, &video4)),
                    &request(VideoHeight::from(1080), &interesting(4, &video3)),
                ],
                no_max
            )
        );

        // And make sure interest doesn't override resolution
        assert_eq!(
            (600, vec![(0x30, 0, 200)]),
            allocate(
                200,
                200,
                0,
                &[
                    &request(VideoHeight::from(1), &interesting(1, &video1)),
                    &request(VideoHeight::from(2), &interesting(2, &video2)),
                    &request(VideoHeight::from(3), &interesting(3, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (600, vec![(0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                400,
                400,
                0,
                &[
                    &request(VideoHeight::from(1), &interesting(1, &video1)),
                    &request(VideoHeight::from(2), &interesting(2, &video2)),
                    &request(VideoHeight::from(3), &interesting(3, &video3)),
                ],
                no_max
            )
        );
        assert_eq!(
            (600, vec![(0x10, 0, 200), (0x20, 0, 200), (0x30, 0, 200)]),
            allocate(
                600,
                600,
                0,
                &[
                    &request(VideoHeight::from(1), &interesting(1, &video1)),
                    &request(VideoHeight::from(2), &interesting(2, &video2)),
                    &request(VideoHeight::from(3), &interesting(3, &video3)),
                ],
                no_max
            )
        );

        let screenshare_layer0 = layer(100, VideoHeight::from(1080));
        let screenshare_layer1 = layer(1500, VideoHeight::from(1080));
        let screenshare_layer2 = layer(0, VideoHeight::from(1080));
        let screenshare = video(
            DemuxId::from_const(0x40),
            [
                &screenshare_layer0,
                &screenshare_layer1,
                &screenshare_layer2,
            ],
        );

        assert_eq!(
            (0, vec![]),
            allocate(
                2000,
                2000,
                0,
                &[&request(VideoHeight::from(0), &screenshare),],
                no_max
            )
        );
        assert_eq!(
            (1500, vec![(0x40, 0, 100)]),
            allocate(
                600,
                600,
                0,
                &[&request(VideoHeight::from(1), &screenshare),],
                no_max
            )
        );
        assert_eq!(
            (1500, vec![(0x40, 1, 1500)]),
            allocate(
                2000,
                2000,
                0,
                &[&request(VideoHeight::from(1), &screenshare),],
                no_max
            )
        );

        // Test with layer 1 using less bandwidth than layer 0
        let small_layer1 = layer(100, VideoHeight::from(360));
        let video_with_small_layer1 =
            video(DemuxId::from_const(0x50), [&layer0, &small_layer1, &layer2]);

        // Not enough bandwidth to allocate anything
        assert_eq!(
            (2000, vec![]),
            allocate(
                99,
                99,
                0,
                &[&request(VideoHeight::from(720), &video_with_small_layer1),],
                no_max
            )
        );

        // Enough for layer 1, even though not enough for layer 0!
        assert_eq!(
            (2000, vec![(0x50, 1, 100)]),
            allocate(
                100,
                100,
                0,
                &[&request(VideoHeight::from(720), &video_with_small_layer1),],
                no_max
            )
        );

        // Enough for layer 2
        assert_eq!(
            (2000, vec![(0x50, 2, 2000)]),
            allocate(
                2000,
                2000,
                0,
                &[&request(VideoHeight::from(720), &video_with_small_layer1),],
                no_max
            )
        );

        // Don't use layer 1, because it's too tall, not enough bandwidth for layer 0
        assert_eq!(
            (200, vec![]),
            allocate(
                100,
                100,
                0,
                &[&request(VideoHeight::from(180), &video_with_small_layer1),],
                no_max
            )
        );

        // Test with layer 2 using less bandwidth than layer 1
        let small_layer2 = layer(100, VideoHeight::from(720));
        let video_with_small_layer2 =
            video(DemuxId::from_const(0x50), [&layer0, &layer1, &small_layer2]);
        let video6 = video(DemuxId::from_const(0x60), [&layer0, &layer1, &layer2]);

        // Enough for layer 2, even though not enough for other layers
        assert_eq!(
            (100, vec![(0x50, 2, 100)]),
            allocate(
                100,
                100,
                0,
                &[&request(VideoHeight::from(720), &video_with_small_layer2),],
                no_max
            )
        );

        // Only enough for the small layer 2
        assert_eq!(
            (2100, vec![(0x50, 2, 100)]),
            allocate(
                100,
                100,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(720), &video6),
                ],
                no_max
            )
        );

        // Enough for everything
        assert_eq!(
            (2100, vec![(0x50, 2, 100), (0x60, 2, 2000)]),
            allocate(
                2100,
                2100,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(720), &video6),
                ],
                no_max
            )
        );

        // Only enough for small layer 2 and other layer 0
        assert_eq!(
            (2100, vec![(0x50, 2, 100), (0x60, 0, 200)]),
            allocate(
                899,
                899,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(720), &video6),
                ],
                no_max
            )
        );

        // Only enough for small layer 2 and other layer 1
        assert_eq!(
            (2100, vec![(0x50, 2, 100), (0x60, 1, 800)]),
            allocate(
                2099,
                2099,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(720), &video6),
                ],
                no_max
            )
        );

        // Only enough for one; biggest wins
        assert_eq!(
            (2100, vec![(0x50, 2, 100)]),
            allocate(
                200,
                200,
                0,
                &[
                    &request(VideoHeight::from(721), &video_with_small_layer2),
                    &request(VideoHeight::from(720), &video6),
                ],
                no_max
            )
        );

        // Only enough for one; biggest wins
        assert_eq!(
            (2100, vec![(0x60, 0, 200)]),
            allocate(
                200,
                200,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(721), &video6),
                ],
                no_max
            )
        );

        // Not enough; small layer 2 and regular layer 1 selected; because
        // both layer 0's would be selected, but 0x50's layer 2 is smallest
        // that fits the requested resolution
        assert_eq!(
            (2100, vec![(0x50, 2, 100), (0x60, 0, 200)]),
            allocate(
                899,
                899,
                0,
                &[
                    &request(VideoHeight::from(720), &video_with_small_layer2),
                    &request(VideoHeight::from(721), &video6),
                ],
                no_max
            )
        );
    }

    fn create_call(call_id: &[u8], now: Instant, system_now: SystemTime) -> Call {
        let creator_id = UserId::from("creator_id".to_string());
        let active_speaker_message_interval = Duration::from_secs(1);
        let initial_target_send_rate = DataRate::from_kbps(600);
        let default_requested_max_send_rate = DataRate::from_kbps(20000);
        Call::new(
            LoggableCallId::from(call_id),
            None,
            creator_id,
            false,
            false,
            active_speaker_message_interval,
            initial_target_send_rate,
            default_requested_max_send_rate,
            now,
            system_now,
            None,
            None,
        )
    }

    fn demux_id_from_unshifted(demux_id_without_shifting: u32) -> DemuxId {
        DemuxId::from_ssrc(demux_id_without_shifting << 4)
    }

    fn add_client(
        call: &mut Call,
        user_id: &str,
        demux_id_without_shifting: u32,
        now: Instant,
    ) -> DemuxId {
        let demux_id = demux_id_from_unshifted(demux_id_without_shifting);
        let user_id = UserId::from(user_id.to_string());
        call.add_client(demux_id, user_id, false, RegionRelation::Unknown, now);
        demux_id
    }

    fn add_admin(
        call: &mut Call,
        user_id: &str,
        demux_id_without_shifting: u32,
        now: Instant,
    ) -> DemuxId {
        let demux_id = demux_id_from_unshifted(demux_id_without_shifting);
        let user_id = UserId::from(user_id.to_string());
        call.add_client(demux_id, user_id, true, RegionRelation::Unknown, now);
        demux_id
    }

    fn create_rtp(
        sender_demux_id: DemuxId,
        layer_id: LayerId,
        seqnum: rtp::FullSequenceNumber,
        payload: &[u8],
    ) -> rtp::Packet<Vec<u8>> {
        let ssrc = layer_id.to_ssrc(sender_demux_id);
        use LayerId::*;
        let pt = match layer_id {
            RtpData => 101,
            Audio => 102,
            Video0 | Video1 | Video2 => 108,
        };
        let timestamp = seqnum as rtp::TruncatedTimestamp;
        // This only gets filled in by the Connection.
        let tcc_seqnum = None;
        rtp::Packet::with_empty_tag(pt, seqnum, timestamp, ssrc, tcc_seqnum, None, payload)
    }

    fn create_data_rtp(
        sender_demux_id: DemuxId,
        seqnum: rtp::FullSequenceNumber,
    ) -> rtp::Packet<Vec<u8>> {
        let layer_id = LayerId::RtpData;
        // The SFU doesn't look at it anyway
        let payload = seqnum.to_be_bytes();
        create_rtp(sender_demux_id, layer_id, seqnum, &payload[..])
    }

    fn create_audio_rtp(
        sender_demux_id: DemuxId,
        seqnum: rtp::FullSequenceNumber,
    ) -> rtp::Packet<Vec<u8>> {
        let layer_id = LayerId::Audio;
        // The SFU doesn't look at it anyway
        let payload = seqnum.to_be_bytes();
        create_rtp(sender_demux_id, layer_id, seqnum, &payload[..])
    }

    fn create_video_rtp(
        sender_demux_id: DemuxId,
        layer_id: LayerId,
        frame_number: u16,
        seqnum: rtp::FullSequenceNumber,
        key_frame_size: Option<PixelSize>,
    ) -> rtp::Packet<Vec<u8>> {
        // Simulate big video packets
        let payload = vec![0; 1200];

        let ssrc = layer_id.to_ssrc(sender_demux_id);
        let pt = 108;
        let timestamp = seqnum as rtp::TruncatedTimestamp;
        rtp::Packet::with_dependency_descriptor(
            pt,
            seqnum,
            timestamp,
            ssrc,
            rtp::DependencyDescriptor {
                is_key_frame: key_frame_size.is_some(),
                resolution: key_frame_size,
                truncated_frame_number: Some(frame_number),
            },
            &payload,
        )
    }

    fn create_server_to_client_rtp(
        seqnum: rtp::FullSequenceNumber,
        payload: &[u8],
    ) -> rtp::Packet<Vec<u8>> {
        let ssrc = CLIENT_SERVER_DATA_SSRC;
        let pt = CLIENT_SERVER_DATA_PAYLOAD_TYPE;
        let tcc_seqnum = None;
        let timestamp = seqnum as rtp::TruncatedTimestamp;
        rtp::Packet::with_empty_tag(pt, seqnum, timestamp, ssrc, tcc_seqnum, None, payload)
    }

    fn create_reliable_server_to_client_rtp(
        seqnum: rtp::FullSequenceNumber,
        payload: &[u8],
    ) -> rtp::Packet<Vec<u8>> {
        let ssrc = CLIENT_SERVER_DATA_SSRC;
        let pt = CLIENT_SERVER_DATA_PAYLOAD_TYPE;
        let tcc_seqnum = None;
        let timestamp = seqnum as rtp::TruncatedTimestamp;
        rtp::Packet::with_empty_tag(pt, seqnum, timestamp, ssrc, tcc_seqnum, None, payload)
    }

    fn create_sfu_to_device(
        joined_or_left: bool,
        active_speaker_id: Option<DemuxId>,
        all_demux_ids: &[DemuxId],
    ) -> protos::SfuToDevice {
        protos::SfuToDevice {
            device_joined_or_left: if joined_or_left {
                Some(protos::sfu_to_device::DeviceJoinedOrLeft::default())
            } else {
                None
            },
            speaker: active_speaker_id.map(|demux_id| protos::sfu_to_device::Speaker {
                demux_id: Some(demux_id.as_u32()),
            }),
            current_devices: Some(protos::sfu_to_device::CurrentDevices {
                demux_ids_with_video: vec![],
                all_demux_ids: all_demux_ids.iter().map(|id| id.as_u32()).collect(),
                allocated_heights: vec![],
            }),
            ..Default::default()
        }
    }

    fn create_resolution_request_rtp(
        demux_id_without_shifting: u32,
        height: u16,
    ) -> rtp::Packet<Vec<u8>> {
        use protos::device_to_sfu::video_request_message::VideoRequest;
        let request = VideoRequest {
            height: Some(height as u32),
            demux_id: Some(demux_id_from_unshifted(demux_id_without_shifting).as_u32()),
        };

        create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                video_request: Some(protos::device_to_sfu::VideoRequestMessage {
                    requests: vec![request],
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        )
    }

    fn create_active_speaker_height_rtp(
        demux_id_without_shifting: u32,
        request_height: u16,
        active_speaker_height: u16,
    ) -> rtp::Packet<Vec<u8>> {
        let request = protos::device_to_sfu::video_request_message::VideoRequest {
            height: Some(request_height as u32),
            demux_id: Some(demux_id_from_unshifted(demux_id_without_shifting).as_u32()),
        };

        create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                video_request: Some(protos::device_to_sfu::VideoRequestMessage {
                    requests: vec![request],
                    active_speaker_height: Some(active_speaker_height as u32),
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        )
    }

    fn create_max_receive_rate_request(max_receive_rate: DataRate) -> rtp::Packet<Vec<u8>> {
        create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                video_request: Some(protos::device_to_sfu::VideoRequestMessage {
                    max_kbps: Some(max_receive_rate.as_kbps() as u32),
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        )
    }

    fn create_leave_rtp() -> rtp::Packet<Vec<u8>> {
        create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                leave: Some(protos::device_to_sfu::LeaveMessage {}),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        )
    }

    fn create_raise_hand_rtp() -> rtp::Packet<Vec<u8>> {
        create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                raise_hand: Some(protos::device_to_sfu::RaiseHand {
                    raise: Some(true),
                    seqnum: Some(1),
                }),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        )
    }

    #[test]
    fn forward_heartbeats() {
        let now = Instant::now();
        let system_now = SystemTime::now();

        let mut call = create_call(b"call_id", now, system_now);

        let sender_demux_id = demux_id_from_unshifted(1);
        let mut rtp1 = create_data_rtp(sender_demux_id, 1);
        assert_eq!(
            Err(Error::UnknownDemuxId(sender_demux_id)),
            call.handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
        );

        let sender_demux_id = add_client(&mut call, "sender", 1, now);
        // It's authorized, but there aren't any receivers to send to.
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        let receiver1_demux_id = add_client(&mut call, "receiver1", 2, now);
        let receiver2_demux_id = add_client(&mut call, "receiver2", 3, now);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp1.clone()),
                (receiver2_demux_id, rtp1.clone())
            ],
            rtp_to_send
        );

        let mut rtp2 = create_data_rtp(sender_demux_id, 2);
        assert_eq!(
            Err(Error::UnauthorizedRtpSsrc(
                sender_demux_id,
                receiver2_demux_id
            )),
            call.handle_rtp(receiver2_demux_id, rtp1.borrow_mut(), now)
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp2.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp2.clone()),
                (receiver2_demux_id, rtp2)
            ],
            rtp_to_send
        );
    }

    #[test]
    fn forward_audio() {
        let now = Instant::now();
        let system_now = SystemTime::now();

        let mut call = create_call(b"call_id", now, system_now);

        let sender_demux_id = demux_id_from_unshifted(1);
        let mut rtp1 = create_audio_rtp(sender_demux_id, 1);
        assert_eq!(
            Err(Error::UnknownDemuxId(sender_demux_id)),
            call.handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
        );

        let sender_demux_id = add_client(&mut call, "sender", 1, now);
        // It's authorized, but there aren't any receivers to send to.
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        let receiver1_demux_id = add_client(&mut call, "receiver1", 2, now);
        let receiver2_demux_id = add_client(&mut call, "receiver2", 3, now);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp1.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp1.clone()),
                (receiver2_demux_id, rtp1.clone())
            ],
            rtp_to_send
        );

        let mut rtp2 = create_audio_rtp(sender_demux_id, 2);
        assert_eq!(
            Err(Error::UnauthorizedRtpSsrc(
                sender_demux_id,
                receiver2_demux_id
            )),
            call.handle_rtp(receiver2_demux_id, rtp1.borrow_mut(), now)
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp2.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp2.clone()),
                (receiver2_demux_id, rtp2)
            ],
            rtp_to_send
        );

        let mut rtp3 = create_audio_rtp(sender_demux_id, 3);
        // Don't forward silence
        rtp3.audio_level = Some(0);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp3.borrow_mut(), now)
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        let mut rtp4 = create_audio_rtp(sender_demux_id, 4);
        rtp4.audio_level = Some(50);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp4.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp4.clone()),
                (receiver2_demux_id, rtp4)
            ],
            rtp_to_send
        );

        let mut rtp5 = create_audio_rtp(sender_demux_id, 4);
        rtp5.audio_level = Some(10);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp5.borrow_mut(), now)
            .unwrap();
        assert_eq!(
            vec![
                (receiver1_demux_id, rtp5.clone()),
                (receiver2_demux_id, rtp5)
            ],
            rtp_to_send
        );
    }

    #[test]
    fn forward_video() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        let sender_demux_id = demux_id_from_unshifted(1);
        let mut frame_number = 101;
        let mut seqnum = 1;
        let size = PixelSize {
            width: 320,
            height: 240,
        };
        let mut rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            frame_number,
            seqnum,
            Some(size),
        );
        assert_eq!(
            Err(Error::UnknownDemuxId(sender_demux_id)),
            call.handle_rtp(sender_demux_id, rtp.borrow_mut(), now)
        );

        let sender_demux_id = add_client(&mut call, "sender", 1, now);
        // It's authorized, but there aren't any receivers to send to.
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(1))
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        // We need at least 2 packets to get the incoming rate working.
        seqnum += 1;
        let mut rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            frame_number,
            seqnum,
            Some(size),
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(2))
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        // This is required to update the incoming rate.
        // We don't trust the incoming rate for 500ms.
        call.tick(at(501));
        assert_eq!(
            Some(DataRate::from_bps(40320)),
            call.clients[0].incoming_video0.rate()
        );
        assert_eq!(
            Some(VideoHeight::from(size.height)),
            call.clients[0].incoming_video0.height
        );

        let receiver1_demux_id = add_client(&mut call, "receiver1", 2, at(502));
        let receiver2_demux_id = add_client(&mut call, "receiver2", 3, at(503));

        frame_number += 1;
        seqnum += 1;
        // Note: This is not a key frame, so it's not forwarded right away
        let mut rtp =
            create_video_rtp(sender_demux_id, LayerId::Video0, frame_number, seqnum, None);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(504))
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        // Need at least 5ms since last tick to generate key frames
        let expected_key_frame_request = (
            sender_demux_id,
            rtp::KeyFrameRequest {
                ssrc: LayerId::Video0.to_ssrc(sender_demux_id),
            },
        );
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(506));
        assert_eq!(
            vec![expected_key_frame_request],
            outgoing_key_frame_requests
        );

        // Still not a key frame.  Keep ignoring.
        seqnum += 1;
        let mut rtp =
            create_video_rtp(sender_demux_id, LayerId::Video0, frame_number, seqnum, None);
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(505))
            .unwrap();
        assert_eq!(0, rtp_to_send.len());

        // And keep asking for a key frame
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(707));
        assert_eq!(
            vec![expected_key_frame_request],
            outgoing_key_frame_requests
        );

        // Now we get a key frame we can forward
        frame_number += 1;
        seqnum += 1;
        let mut rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            frame_number,
            seqnum,
            Some(size),
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(708))
            .unwrap();

        let rewritten_frame_number = 2;
        let rewritten_timestamp = 1;
        let rewritten_seqnum = 2;
        let mut rewritten_rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            rewritten_frame_number,
            rewritten_timestamp,
            Some(size),
        );
        rewritten_rtp.set_seqnum_in_header(rewritten_seqnum);
        assert_eq!(
            vec![
                (receiver1_demux_id, rewritten_rtp.clone()),
                (receiver2_demux_id, rewritten_rtp),
            ],
            rtp_to_send
        );

        // And we don't ask for a key frame any more
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(908));
        assert_eq!(0, outgoing_key_frame_requests.len());

        // Unless the client requests one
        call.handle_key_frame_requests(
            receiver1_demux_id,
            &[expected_key_frame_request.1],
            at(908),
        );
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(1110));
        assert_eq!(
            vec![expected_key_frame_request],
            outgoing_key_frame_requests
        );

        // Get the sender sending a higher layer
        let mut frame_number_layer1 = 201;
        let mut seqnum_layer1 = 0;
        let size_layer1 = PixelSize {
            width: 640,
            height: 480,
        };
        // We need at least 3 packets to get the incoming rate working above the lower layer.
        for _ in 0..3 {
            seqnum_layer1 += 1;
            let mut rtp_layer1 = create_video_rtp(
                sender_demux_id,
                LayerId::Video1,
                frame_number_layer1,
                seqnum_layer1,
                Some(size_layer1),
            );
            let rtp_to_send = call
                .handle_rtp(sender_demux_id, rtp_layer1.borrow_mut(), at(2000))
                .unwrap();
            // No one has requested it yet
            assert_eq!(0, rtp_to_send.len());
        }

        // This is required to update the incoming rate.
        // We don't trust the incoming rate for 500ms.
        call.tick(at(2500));
        assert_eq!(
            Some(DataRate::from_bps(60480)),
            call.clients[0].incoming_video1.rate()
        );
        assert_eq!(
            Some(VideoHeight::from(size_layer1.height)),
            call.clients[0].incoming_video1.height
        );

        let mut resolution_request = create_resolution_request_rtp(1, 480);

        // When a receiver requests a resolution high enough for layer1, it triggers key frame request for that layer,
        call.handle_rtp(
            receiver1_demux_id,
            resolution_request.borrow_mut(),
            at(2801),
        )
        .unwrap();
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(2801));
        let expected_key_frame_request_layer1 = (
            sender_demux_id,
            rtp::KeyFrameRequest {
                ssrc: LayerId::Video1.to_ssrc(sender_demux_id),
            },
        );
        assert_eq!(
            Some(expected_key_frame_request_layer1.1.ssrc),
            call.clients[1]
                .video_forwarder_by_sender_demux_id
                .get(&sender_demux_id)
                .unwrap()
                .needs_key_frame()
        );
        assert_eq!(
            vec![expected_key_frame_request_layer1],
            outgoing_key_frame_requests
        );

        // But the lower layer is still forwarded until a key frame for the higher layer comes in.
        frame_number += 1;
        seqnum += 1;
        let mut rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            frame_number,
            seqnum,
            Some(size),
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp.borrow_mut(), at(2802))
            .unwrap();

        let rewritten_frame_number = 3;
        let rewritten_timestamp = 2;
        let rewritten_seqnum = 3;
        let mut rewritten_rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            rewritten_frame_number,
            rewritten_timestamp,
            Some(size),
        );
        rewritten_rtp.set_seqnum_in_header(rewritten_seqnum);
        assert_eq!(
            vec![
                (receiver1_demux_id, rewritten_rtp.clone()),
                (receiver2_demux_id, rewritten_rtp),
            ],
            rtp_to_send
        );

        frame_number_layer1 += 1;
        let mut rtp_layer1 = create_video_rtp(
            sender_demux_id,
            LayerId::Video1,
            frame_number_layer1,
            seqnum_layer1,
            Some(size_layer1),
        );
        let rtp_to_send = call
            .handle_rtp(sender_demux_id, rtp_layer1.borrow_mut(), at(2803))
            .unwrap();
        let rewritten_frame_number = 4;
        let rewritten_timestamp = 3;
        let rewritten_seqnum = 5;
        let mut rewritten_rtp = create_video_rtp(
            sender_demux_id,
            LayerId::Video0,
            rewritten_frame_number,
            rewritten_timestamp,
            Some(size_layer1),
        );
        rewritten_rtp.set_seqnum_in_header(rewritten_seqnum);
        assert_eq!(vec![(receiver1_demux_id, rewritten_rtp),], rtp_to_send);

        // If the incoming bitrate rate gets higher than the target send rate, drop back to the base layer
        for _ in 0..900 {
            seqnum_layer1 += 1;
            let mut rtp_layer1 = create_video_rtp(
                sender_demux_id,
                LayerId::Video1,
                frame_number_layer1,
                seqnum_layer1,
                Some(size_layer1),
            );
            call.handle_rtp(sender_demux_id, rtp_layer1.borrow_mut(), at(4000))
                .unwrap();
        }
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(4000));
        assert_eq!(vec![] as Vec<(DemuxId, _)>, outgoing_key_frame_requests);

        for _ in 0..900 {
            seqnum_layer1 += 1;
            let mut rtp_layer1 = create_video_rtp(
                sender_demux_id,
                LayerId::Video1,
                frame_number_layer1,
                seqnum_layer1,
                Some(size_layer1),
            );
            call.handle_rtp(sender_demux_id, rtp_layer1.borrow_mut(), at(5000))
                .unwrap();
        }

        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(5100));
        dbg!(call.clients[0].incoming_video1.rate().unwrap().as_bps());
        assert_eq!(
            Some(DataRate::from_bps(1043790)),
            call.clients[0].incoming_video1.rate()
        );
        assert_eq!(
            Some(expected_key_frame_request.1.ssrc),
            call.clients[1]
                .video_forwarder_by_sender_demux_id
                .get(&sender_demux_id)
                .unwrap()
                .needs_key_frame()
        );
        assert_eq!(
            vec![expected_key_frame_request],
            outgoing_key_frame_requests
        );

        assert_eq!(
            vec![
                SendRateAllocationInfo {
                    demux_id: sender_demux_id,
                    padding_ssrc: Some(LayerId::Video0.to_rtx_ssrc(receiver1_demux_id)),
                    target_send_rate: DataRate::from_kbps(600),
                    requested_base_rate: DataRate::default(),
                    ideal_send_rate: DataRate::from_bps(0),
                },
                SendRateAllocationInfo {
                    demux_id: receiver1_demux_id,
                    padding_ssrc: Some(LayerId::Video0.to_rtx_ssrc(sender_demux_id)),
                    target_send_rate: DataRate::from_kbps(600),
                    requested_base_rate: DataRate::from_bps(34546),
                    ideal_send_rate: DataRate::from_bps(1043790),
                },
                SendRateAllocationInfo {
                    demux_id: receiver2_demux_id,
                    padding_ssrc: Some(LayerId::Video0.to_rtx_ssrc(sender_demux_id)),
                    target_send_rate: DataRate::from_kbps(600),
                    requested_base_rate: DataRate::from_bps(34546),
                    ideal_send_rate: DataRate::from_bps(34546),
                }
            ],
            call.get_send_rate_allocation_info().collect::<Vec<_>>()
        );
    }

    #[test]
    fn send_updates_when_someone_joins_or_leaves() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        // It's a little weird that you get updates for when you join, but it doesn't really do any harm and it's much easier to implement.
        let demux_id1 = add_client(&mut call, "1", 1, at(99));

        let expected_update_payload_just_client1 =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(100));
        assert_eq!(
            vec![(
                demux_id1,
                create_server_to_client_rtp(1, &expected_update_payload_just_client1)
            )],
            rtp_to_send
        );

        let demux_id2 = add_client(&mut call, "2", 2, at(200));

        let expected_update_payload_both_clients =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id2]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(200));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(2, &expected_update_payload_both_clients)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(1, &expected_update_payload_both_clients)
                )
            ],
            rtp_to_send
        );

        // Nothing is sent out because nothing changed.
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(300));
        assert_eq!(0, rtp_to_send.len());

        call.drop_client(demux_id1, at(400));

        let expected_update_payload_just_client2 = create_sfu_to_device(
            true,
            Some(demux_id1), // Is it okay that the active speaker left?
            &[demux_id2],
        )
        .encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(400));
        assert_eq!(
            vec![(
                demux_id2,
                create_server_to_client_rtp(2, &expected_update_payload_just_client2)
            )],
            rtp_to_send
        );
    }

    #[test]
    fn send_updates_for_pending_clients() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        // It's a little weird that you get updates for when you join, but it doesn't really do any harm and it's much easier to implement.
        let demux_id1 = add_client(&mut call, "1", 1, at(99));

        let expected_update_payload_just_client1 =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1]).encode_to_vec();
        let expected_update_payload_for_pending = protos::SfuToDevice {
            device_joined_or_left: Some(protos::sfu_to_device::DeviceJoinedOrLeft {}),
            ..Default::default()
        }
        .encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(100));
        assert_eq!(
            vec![(
                demux_id1,
                create_server_to_client_rtp(1, &expected_update_payload_just_client1)
            )],
            rtp_to_send
        );

        call.new_clients_require_approval = true;
        let demux_id2 = add_client(&mut call, "2", 2, at(200));

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(200));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(2, &expected_update_payload_just_client1)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(1, &expected_update_payload_for_pending)
                )
            ],
            rtp_to_send
        );

        call.approve_pending_client(demux_id2, at(300));

        let expected_update_payload_both_clients =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id2]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(300));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(3, &expected_update_payload_both_clients)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(2, &expected_update_payload_both_clients)
                )
            ],
            rtp_to_send
        );

        call.drop_client(demux_id2, at(400));

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(400));
        assert_eq!(
            vec![(
                demux_id1,
                create_server_to_client_rtp(4, &expected_update_payload_just_client1)
            )],
            rtp_to_send
        );

        // Re-add the same user.
        let demux_id22 = add_client(&mut call, "2", 22, at(500));

        let expected_update_payload_both_clients_after_rejoin =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id22]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(500));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(
                        5,
                        &expected_update_payload_both_clients_after_rejoin
                    )
                ),
                (
                    demux_id22,
                    create_server_to_client_rtp(
                        1,
                        &expected_update_payload_both_clients_after_rejoin
                    )
                )
            ],
            rtp_to_send
        );
    }

    trait HasDemuxId {
        fn demux_id(&self) -> DemuxId;
    }
    impl HasDemuxId for Client {
        fn demux_id(&self) -> DemuxId {
            self.demux_id
        }
    }
    impl HasDemuxId for NonParticipantClient {
        fn demux_id(&self) -> DemuxId {
            self.demux_id
        }
    }

    fn demux_ids(clients: &[impl HasDemuxId]) -> Vec<DemuxId> {
        clients.iter().map(|client| client.demux_id()).collect()
    }

    #[test]
    fn admin_client_is_not_pending() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let _non_admin = add_client(&mut call, "1", 1, at(100));
        assert_eq!(0, call.clients.len());
        let admin = add_admin(&mut call, "2", 2, at(200));
        assert_eq!(vec![admin], demux_ids(&call.clients));
    }

    #[test]
    fn approve_multiple_clients_with_same_user_id() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let client_device_1 = add_client(&mut call, "Them", 1, at(100));
        let other_device = add_client(&mut call, "Somebody Else", 2, at(200));
        let client_device_2 = add_client(&mut call, "Them", 3, at(300));
        assert_eq!(
            vec![client_device_1, other_device, client_device_2],
            demux_ids(&call.pending_clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        call.approve_pending_client(client_device_2, at(400));

        assert_eq!(vec![other_device], demux_ids(&call.pending_clients));
        assert_eq!(
            vec![client_device_1, client_device_2],
            demux_ids(&call.clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
    }

    #[test]
    fn deny_multiple_clients_with_same_user_id() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let client_device_1 = add_client(&mut call, "Them", 1, at(100));
        let other_device = add_client(&mut call, "Somebody Else", 2, at(200));
        let client_device_2 = add_client(&mut call, "Them", 3, at(300));
        assert_eq!(
            vec![client_device_1, other_device, client_device_2],
            demux_ids(&call.pending_clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        call.deny_pending_client(client_device_2, at(400));

        assert_eq!(vec![other_device], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![client_device_1, client_device_2],
            demux_ids(&call.removed_clients)
        );
    }

    #[test]
    fn pending_client_can_leave() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let non_admin = add_client(&mut call, "1", 1, at(100));
        assert_eq!(vec![non_admin], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        let result = call.handle_rtp(non_admin, create_leave_rtp().borrow_mut(), at(200));
        assert_eq!(Error::Leave, result.unwrap_err());
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
    }

    #[test]
    fn removed_client_can_leave() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        let non_admin = add_client(&mut call, "1", 1, at(100));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![non_admin], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        call.force_remove_client(non_admin, at(200));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![non_admin], demux_ids(&call.removed_clients));

        let result = call.handle_rtp(non_admin, create_leave_rtp().borrow_mut(), at(300));
        assert_eq!(Error::Leave, result.unwrap_err());
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
    }

    #[test]
    fn send_updates_for_removed_clients() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        // It's a little weird that you get updates for when you join, but it doesn't really do any harm and it's much easier to implement.
        let demux_id1 = add_client(&mut call, "1", 1, at(99));

        let expected_update_payload_just_client1 =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(100));
        assert_eq!(
            vec![(
                demux_id1,
                create_server_to_client_rtp(1, &expected_update_payload_just_client1)
            )],
            rtp_to_send
        );

        let demux_id2 = add_client(&mut call, "2", 2, at(200));

        let expected_update_payload_both_clients =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id2]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(200));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(2, &expected_update_payload_both_clients)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(1, &expected_update_payload_both_clients)
                )
            ],
            rtp_to_send
        );

        call.force_remove_client(demux_id2, at(300));
        let expected_update_payload_for_removed = protos::SfuToDevice {
            removed: Some(protos::sfu_to_device::Removed::default()),
            ..Default::default()
        }
        .encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(300));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(3, &expected_update_payload_just_client1)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(2, &expected_update_payload_for_removed)
                )
            ],
            rtp_to_send
        );

        call.drop_client(demux_id2, at(400));

        // Clients leaving the "removed" list don't count as participant changes,
        // so there's no update for client 1 at this tick.
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(400));
        assert_eq!(0, rtp_to_send.len());

        // Re-add the same user.
        let demux_id22 = add_client(&mut call, "2", 22, at(500));

        let expected_update_payload_both_clients_after_rejoin =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id22]).encode_to_vec();

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(500));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(
                        4,
                        &expected_update_payload_both_clients_after_rejoin
                    )
                ),
                (
                    demux_id22,
                    create_server_to_client_rtp(
                        1,
                        &expected_update_payload_both_clients_after_rejoin
                    )
                )
            ],
            rtp_to_send
        );
    }

    #[test]
    fn blocked_clients_cannot_rejoin() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_device_1 = add_client(&mut call, "Alice", 1, at(100));
        let alice_device_2 = add_client(&mut call, "Alice", 2, at(200));
        let bob_device_1 = add_client(&mut call, "Bob", 11, at(300));
        let bob_device_2 = add_client(&mut call, "Bob", 12, at(400));
        assert_eq!(
            vec![alice_device_1, alice_device_2, bob_device_1, bob_device_2],
            demux_ids(&call.pending_clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        call.approve_pending_client(alice_device_1, at(500));
        call.approve_pending_client(bob_device_2, at(600));

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, bob_device_1, bob_device_2],
            demux_ids(&call.clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        call.block_client(alice_device_1, at(700));

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![bob_device_1, bob_device_2], demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.removed_clients)
        );

        let alice_device_3 = add_client(&mut call, "Alice", 3, at(800));

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![bob_device_1, bob_device_2], demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, alice_device_3],
            demux_ids(&call.removed_clients)
        );

        // By contrast, regular removal is by device and allows re-adding.
        call.force_remove_client(bob_device_2, at(900));

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![bob_device_1], demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, alice_device_3, bob_device_2],
            demux_ids(&call.removed_clients)
        );

        let bob_device_3 = add_client(&mut call, "Bob", 13, at(1000));

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![bob_device_1, bob_device_3], demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, alice_device_3, bob_device_2],
            demux_ids(&call.removed_clients)
        );
    }

    #[test]
    fn repeated_deny_results_in_block() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_user_id = UserId::from("Alice".to_string());

        let alice_device_1 = add_client(&mut call, alice_user_id.as_str(), 1, at(100));
        assert_eq!(vec![alice_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.denied_users.is_empty());
        assert!(call.blocked_users.is_empty());

        call.deny_pending_client(alice_device_1, at(200));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());

        // A second "deny" of the same demux ID does not count, because that device is no longer pending.
        call.deny_pending_client(alice_device_1, at(250));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());

        let alice_device_2 = add_client(&mut call, alice_user_id.as_str(), 2, at(300));
        assert_eq!(vec![alice_device_2], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());

        call.deny_pending_client(alice_device_2, at(400));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.removed_clients)
        );
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.contains(&alice_user_id));

        let alice_device_3 = add_client(&mut call, alice_user_id.as_str(), 3, at(500));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, alice_device_3],
            demux_ids(&call.removed_clients)
        );
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.contains(&alice_user_id));
    }

    #[test]
    fn removal_resets_approval() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_user_id = UserId::from("Alice".to_string());

        let alice_device_1 = add_client(&mut call, alice_user_id.as_str(), 1, at(100));
        assert_eq!(vec![alice_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        call.approve_pending_client(alice_device_1, at(200));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));

        call.force_remove_client(alice_device_1, at(300));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        let alice_device_2 = add_client(&mut call, alice_user_id.as_str(), 2, at(400));
        assert_eq!(vec![alice_device_2], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());
    }

    #[test]
    fn removal_retains_approval_if_active_device_remains() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_user_id = UserId::from("Alice".to_string());

        let alice_device_1 = add_client(&mut call, alice_user_id.as_str(), 1, at(100));
        assert_eq!(vec![alice_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        let alice_device_2 = add_client(&mut call, alice_user_id.as_str(), 2, at(200));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.pending_clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        call.approve_pending_client(alice_device_1, at(300));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.clients)
        );
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));

        call.force_remove_client(alice_device_1, at(400));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_2], demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));

        let alice_device_3 = add_client(&mut call, alice_user_id.as_str(), 3, at(500));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(
            vec![alice_device_2, alice_device_3],
            demux_ids(&call.clients)
        );
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));
    }

    #[test]
    fn blocking_resets_approval() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_user_id = UserId::from("Alice".to_string());

        let alice_device_1 = add_client(&mut call, alice_user_id.as_str(), 1, at(100));
        assert_eq!(vec![alice_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        call.approve_pending_client(alice_device_1, at(200));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));

        call.block_client(alice_device_1, at(300));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());

        let alice_device_2 = add_client(&mut call, alice_user_id.as_str(), 2, at(400));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.removed_clients)
        );
        assert!(call.approved_users.is_empty());
    }

    #[test]
    fn approval_resets_denial() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_user_id = UserId::from("Alice".to_string());

        let alice_device_1 = add_client(&mut call, alice_user_id.as_str(), 1, at(100));
        assert_eq!(vec![alice_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.is_empty());
        assert!(call.blocked_users.is_empty());

        call.deny_pending_client(alice_device_1, at(200));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());

        let alice_device_2 = add_client(&mut call, alice_user_id.as_str(), 2, at(300));
        assert_eq!(vec![alice_device_2], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());

        call.approve_pending_client(alice_device_2, at(400));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_2], demux_ids(&call.clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.removed_clients));
        assert!(call.approved_users.contains(&alice_user_id));
        assert!(call.denied_users.is_empty());
        assert!(call.blocked_users.is_empty());

        call.force_remove_client(alice_device_2, at(500));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.removed_clients)
        );
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.is_empty());
        assert!(call.blocked_users.is_empty());

        let alice_device_3 = add_client(&mut call, alice_user_id.as_str(), 3, at(500));
        assert_eq!(vec![alice_device_3], demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2],
            demux_ids(&call.removed_clients)
        );
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.is_empty());
        assert!(call.blocked_users.is_empty());

        call.deny_pending_client(alice_device_3, at(600));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.clients));
        assert_eq!(
            vec![alice_device_1, alice_device_2, alice_device_3],
            demux_ids(&call.removed_clients)
        );
        assert!(call.approved_users.is_empty());
        assert!(call.denied_users.contains(&alice_user_id));
        assert!(call.blocked_users.is_empty());
    }

    #[test]
    fn send_active_speaker_updates() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        let demux_id1 = add_client(&mut call, "1", 1, at(1));
        let demux_id2 = add_client(&mut call, "2", 2, at(2));
        // If there is no audio activity from anyone, we choose the first client as the active speaker
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(301));

        let expected_update_payload =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id1, demux_id2]).encode_to_vec();
        assert_eq!(Some(demux_id1), call.active_speaker_id);
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(1, &expected_update_payload)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(1, &expected_update_payload)
                )
            ],
            rtp_to_send
        );

        // Switch to demux_id2 as active speaker and send out an update.
        for seqnum in 1..100 {
            let mut rtp = create_audio_rtp(demux_id2, seqnum);
            // We can't just send 100 every time or that becomes the noise floor
            rtp.audio_level = Some(seqnum as u8);
            let _rtp_to_send = call.handle_rtp(demux_id2, rtp.borrow_mut(), at(301 + seqnum));
        }
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(602));

        let expected_update_payload =
            create_sfu_to_device(false, Some(demux_id2), &[demux_id1, demux_id2]).encode_to_vec();
        assert_eq!(Some(demux_id2), call.active_speaker_id);
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(2, &expected_update_payload)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(2, &expected_update_payload)
                )
            ],
            rtp_to_send
        );

        // Switch to demux_id1 as active speaker and send out an update.
        for seqnum in 1..100 {
            let mut rtp = create_audio_rtp(demux_id1, seqnum);
            // We can't just send 100 every time or that becomes the noise floor
            rtp.audio_level = Some(seqnum as u8);
            let _rtp_to_send = call.handle_rtp(demux_id1, rtp.borrow_mut(), at(602 + seqnum));

            let mut rtp = create_audio_rtp(demux_id2, seqnum);
            rtp.audio_level = Some(0);
            let _rtp_to_send = call.handle_rtp(demux_id2, rtp.borrow_mut(), at(602 + seqnum));
        }
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(903));

        let expected_update_payload =
            create_sfu_to_device(false, Some(demux_id1), &[demux_id1, demux_id2]).encode_to_vec();
        assert_eq!(Some(demux_id1), call.active_speaker_id);
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(3, &expected_update_payload)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(3, &expected_update_payload)
                )
            ],
            rtp_to_send
        );

        let get_stats = |from_server: &[RtpToSend],
                         receiver_demux_id: DemuxId|
         -> Option<protos::sfu_to_device::Stats> {
            let (_demux_id, rtp) = from_server
                .iter()
                .find(|(demux_id, _rtp)| *demux_id == receiver_demux_id)?;
            let proto = protos::SfuToDevice::decode(rtp.payload()).ok()?;
            proto.stats
        };

        // Don't resend anything after just 301ms (except stats)
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(1204));
        assert_eq!(2, rtp_to_send.len());
        assert_eq!(
            Some(protos::sfu_to_device::Stats {
                target_send_rate_kbps: Some(600),
                ideal_send_rate_kbps: Some(0),
                allocated_send_rate_kbps: Some(0),
            }),
            get_stats(&rtp_to_send, demux_id1)
        );
        assert_eq!(
            Some(protos::sfu_to_device::Stats {
                target_send_rate_kbps: Some(600),
                ideal_send_rate_kbps: Some(0),
                allocated_send_rate_kbps: Some(0),
            }),
            get_stats(&rtp_to_send, demux_id2)
        );

        // But do resend after 1001ms
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(1904));
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(5, &expected_update_payload)
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(5, &expected_update_payload)
                )
            ],
            rtp_to_send
        );

        // And more stats a little later.
        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(2205));
        assert_eq!(2, rtp_to_send.len());
        assert_eq!(
            Some(protos::sfu_to_device::Stats {
                target_send_rate_kbps: Some(600),
                ideal_send_rate_kbps: Some(0),
                allocated_send_rate_kbps: Some(0),
            }),
            get_stats(&rtp_to_send, demux_id1)
        );
        assert_eq!(
            Some(protos::sfu_to_device::Stats {
                target_send_rate_kbps: Some(600),
                ideal_send_rate_kbps: Some(0),
                allocated_send_rate_kbps: Some(0),
            }),
            get_stats(&rtp_to_send, demux_id2)
        );
    }

    #[test]
    fn send_key_frame_request_on_active_speaker_change() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        let demux_id1 = add_client(&mut call, "1", 1, at(1));
        let demux_id2 = add_client(&mut call, "2", 2, at(2));
        // If there is no audio activity from anyone, we choose the first client as the active speaker
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(301));

        assert_eq!(Some(demux_id1), call.active_speaker_id);

        // There are no outgoing key frame requests when the active speaker changed because the
        // active speaker height hasn't been specified by any clients.
        assert_eq!(0, outgoing_key_frame_requests.len());

        let mut resolution_request = create_active_speaker_height_rtp(1, 120, 480);

        call.handle_rtp(demux_id1, resolution_request.borrow_mut(), at(302))
            .unwrap();

        // Receiving low resolution video from demux_id2 which is smaller in height than the
        // active speaker is displayed at (on demux_id1's device).
        let mut rtp = create_video_rtp(
            demux_id2,
            LayerId::Video0,
            101,
            1,
            Some(PixelSize {
                width: 320,
                height: 240,
            }),
        );
        call.handle_rtp(demux_id2, rtp.borrow_mut(), at(303))
            .unwrap();

        // Switch to demux_id2 as active speaker and send out an update.
        for seqnum in 1..100 {
            let mut rtp = create_audio_rtp(demux_id2, seqnum);
            // We can't just send 100 every time or that becomes the noise floor
            rtp.audio_level = Some(seqnum as u8);
            let _rtp_to_send = call.handle_rtp(demux_id2, rtp.borrow_mut(), at(302 + seqnum));
        }
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(603));

        assert_eq!(Some(demux_id2), call.active_speaker_id);

        // Request key frames from demux_id2 since they're now the active speaker, and demux_id1
        // will start viewing them in a larger view soon.
        assert_eq!(
            outgoing_key_frame_requests,
            &[
                (
                    demux_id2,
                    rtp::KeyFrameRequest {
                        ssrc: LayerId::Video1.to_ssrc(demux_id2),
                    }
                ),
                (
                    demux_id2,
                    rtp::KeyFrameRequest {
                        ssrc: LayerId::Video2.to_ssrc(demux_id2),
                    }
                )
            ]
        );

        let mut resolution_request = create_active_speaker_height_rtp(1, 120, 200);

        call.handle_rtp(demux_id2, resolution_request.borrow_mut(), at(604))
            .unwrap();

        // The lowest layer video received from demux_id1 is larger than the active speaker is
        // viewed at on demux_id2's device.
        let mut rtp = create_video_rtp(
            demux_id1,
            LayerId::Video0,
            101,
            1,
            Some(PixelSize {
                width: 320,
                height: 240,
            }),
        );
        call.handle_rtp(demux_id1, rtp.borrow_mut(), at(605))
            .unwrap();

        // Switch to demux_id1 as active speaker and send out an update.
        for seqnum in 1..100 {
            let mut rtp = create_audio_rtp(demux_id1, seqnum);
            // We can't just send 100 every time or that becomes the noise floor
            rtp.audio_level = Some(seqnum as u8);
            let _rtp_to_send = call.handle_rtp(demux_id1, rtp.borrow_mut(), at(605 + seqnum));

            let mut rtp = create_audio_rtp(demux_id2, seqnum);
            rtp.audio_level = Some(0);
            let _rtp_to_send = call.handle_rtp(demux_id2, rtp.borrow_mut(), at(605 + seqnum));
        }
        let (_rtp_to_send, outgoing_key_frame_requests) = call.tick(at(906));

        assert_eq!(Some(demux_id1), call.active_speaker_id);

        // The lowest layer is good enough for demux_id2 already, so no key frame requests are sent
        // there. demux_id1 isn't sent any key frame requests either despite having a larger active
        // speaker height because a client doesn't need to request key frames for themselves.
        assert_eq!(0, outgoing_key_frame_requests.len());
    }

    #[test]
    fn send_forwarding_video_updates() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);
        let get_forwarding_video_demux_ids =
            |from_server: &[RtpToSend], receiver_demux_id: DemuxId| -> Option<Vec<DemuxId>> {
                let (_demux_id, rtp) = from_server
                    .iter()
                    .find(|(demux_id, _rtp)| *demux_id == receiver_demux_id)?;
                let proto = protos::SfuToDevice::decode(rtp.payload()).ok()?;
                let mut demux_ids: Vec<DemuxId> = proto
                    .current_devices?
                    .demux_ids_with_video
                    .iter()
                    .map(|demux_id| DemuxId::try_from(*demux_id).unwrap())
                    .collect();
                demux_ids.sort();
                Some(demux_ids)
            };

        let mut call = create_call(b"call_id", now, system_now);
        let demux_id1 = add_client(&mut call, "1", 1, at(1));
        let demux_id2 = add_client(&mut call, "2", 2, at(2));
        let demux_id3 = add_client(&mut call, "3", 3, at(3));

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(4));
        // Nothing to forward yet
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );

        // Send some video from client2 so the incoming rate goes up.
        for seqnum in 0..10 {
            let mut to_server = create_video_rtp(
                demux_id2,
                LayerId::Video0,
                1,
                seqnum,
                Some(PixelSize {
                    width: 640,
                    height: 480,
                }),
            );
            call.handle_rtp(demux_id2, to_server.borrow_mut(), at(5))
                .unwrap();
        }

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(1006));
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );

        // Make sure we keep forwarding even after getting a key frame
        let mut to_server = create_video_rtp(
            demux_id2,
            LayerId::Video0,
            1,
            11,
            Some(PixelSize {
                width: 640,
                height: 480,
            }),
        );
        call.handle_rtp(demux_id2, to_server.borrow_mut(), at(1007))
            .unwrap();

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(2008));
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );

        // Request a really low max recv rate to prevent things from being forwarded
        let mut to_server = create_max_receive_rate_request(DataRate::from_kbps(1));
        call.handle_rtp(demux_id1, to_server.borrow_mut(), at(2009))
            .unwrap();

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(3010));
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );
    }

    #[test]
    fn allocated_height_updates() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);
        let get_demux_ids_and_heights = |from_server: &[RtpToSend],
                                         receiver_demux_id: DemuxId|
         -> Option<Vec<(DemuxId, u32)>> {
            let (_demux_id, rtp) = from_server
                .iter()
                .find(|(demux_id, _rtp)| *demux_id == receiver_demux_id)?;
            let proto = protos::SfuToDevice::decode(rtp.payload()).ok()?;
            let current_devices = proto.current_devices?;
            let mut demux_ids_and_heights: Vec<(DemuxId, u32)> = current_devices
                .demux_ids_with_video
                .iter()
                .zip(current_devices.allocated_heights.iter())
                .map(|(demux_id, height)| (DemuxId::try_from(*demux_id).unwrap(), *height))
                .collect();
            demux_ids_and_heights.sort();
            Some(demux_ids_and_heights)
        };

        let mut call = create_call(b"call_id", now, system_now);
        let demux_id1 = add_client(&mut call, "1", 1, at(1));
        let demux_id2 = add_client(&mut call, "2", 2, at(2));

        let mut resolution_request = create_resolution_request_rtp(1, 240);
        call.handle_rtp(demux_id1, resolution_request.borrow_mut(), at(3))
            .unwrap();

        let mut resolution_request = create_resolution_request_rtp(2, 240);
        call.handle_rtp(demux_id2, resolution_request.borrow_mut(), at(4))
            .unwrap();

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(5));

        // No heights are allocated yet because no video is being sent yet.
        assert_eq!(
            Some(vec![]),
            get_demux_ids_and_heights(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_demux_ids_and_heights(&from_server, demux_id2)
        );

        // Switch to demux_id2 as active speaker and send out an update.
        for seqnum in 1..100 {
            let mut rtp = create_audio_rtp(demux_id2, seqnum);
            // We can't just send 100 every time or that becomes the noise floor
            rtp.audio_level = Some(seqnum as u8);
            let _rtp_to_send = call.handle_rtp(demux_id2, rtp.borrow_mut(), at(305 + seqnum));
        }
        let (_from_server, _outgoing_key_frame_requests) = call.tick(at(605));

        // Send some video from demux_id2 so that there's video to forward.
        for seqnum in 0..10 {
            let mut to_server = create_video_rtp(
                demux_id2,
                LayerId::Video0,
                1,
                seqnum * 2,
                Some(PixelSize {
                    width: 320,
                    height: 240,
                }),
            );
            call.handle_rtp(demux_id2, to_server.borrow_mut(), at(606))
                .unwrap();

            let mut to_server = create_video_rtp(
                demux_id2,
                LayerId::Video1,
                2,
                (seqnum + 1) * 2,
                Some(PixelSize {
                    width: 640,
                    height: 480,
                }),
            );
            call.handle_rtp(demux_id2, to_server.borrow_mut(), at(606))
                .unwrap();
        }

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(1607));
        assert_eq!(
            Some(vec![(demux_id2, 240)]),
            get_demux_ids_and_heights(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_demux_ids_and_heights(&from_server, demux_id2)
        );

        // The active speaker's height increases in demux_id1's UI, so allocate the higher video layer.
        let mut resolution_request = create_active_speaker_height_rtp(1, 240, 480);
        call.handle_rtp(demux_id1, resolution_request.borrow_mut(), at(1608))
            .unwrap();

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(2709));
        assert_eq!(
            Some(vec![(demux_id2, 480)]),
            get_demux_ids_and_heights(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_demux_ids_and_heights(&from_server, demux_id2)
        );

        // demux_id2 leaves, so there's no height allocated for demux_id1 anymore.
        assert_eq!(
            call.handle_rtp(demux_id2, create_leave_rtp().borrow_mut(), at(3710)),
            Err(Error::Leave)
        );

        let mut empty_resolution_request = create_server_to_client_rtp(
            1,
            protos::DeviceToSfu {
                video_request: Some(protos::device_to_sfu::VideoRequestMessage {
                    requests: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec()
            .as_slice(),
        );
        call.handle_rtp(demux_id1, empty_resolution_request.borrow_mut(), at(4711))
            .unwrap();

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(5712));

        assert_eq!(
            Some(vec![]),
            get_demux_ids_and_heights(&from_server, demux_id1)
        );
    }

    #[test]
    fn test_leave_message() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        let demux_id1 = add_client(&mut call, "1", 1, at(99));
        let demux_id2 = add_client(&mut call, "2", 2, at(200));
        assert_eq!(2, call.clients.len());

        // Clear out updates.
        let (_rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(300));

        assert_eq!(
            call.handle_rtp(demux_id1, create_leave_rtp().borrow_mut(), at(400)),
            Err(Error::Leave)
        );
        assert_eq!(1, call.clients.len());

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(400));
        let expected_update_payload =
            create_sfu_to_device(true, Some(demux_id1), &[demux_id2]).encode_to_vec();
        assert_eq!(
            vec![(
                demux_id2,
                create_server_to_client_rtp(2, &expected_update_payload)
            )],
            rtp_to_send
        );
    }

    #[test]
    fn test_raise_hand_message() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        let demux_id1 = add_client(&mut call, "1", 1, at(99));
        let demux_id2 = add_client(&mut call, "2", 2, at(200));
        assert_eq!(2, call.clients.len());

        // Clear out updates.
        let (_rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(300));

        let rtp_to_send = call
            .handle_rtp(demux_id1, create_raise_hand_rtp().borrow_mut(), at(400))
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(2, call.clients.len());

        let (rtp_to_send, _outgoing_key_frame_requests) = call.tick(at(400));

        // Client1 should have a target_seqnum of 1
        let expected_update_payload_for_raised_hands_client1 = protos::SfuToDevice {
            raised_hands: Some(protos::sfu_to_device::RaisedHands {
                demux_ids: vec![demux_id1.as_u32()],
                seqnums: vec![1],
                target_seqnum: Some(1),
            }),
            ..Default::default()
        }
        .encode_to_vec();

        // Client2 should have a target_seqnum of 0
        let expected_update_payload_for_raised_hands_client2 = protos::SfuToDevice {
            raised_hands: Some(protos::sfu_to_device::RaisedHands {
                demux_ids: vec![demux_id1.as_u32()],
                seqnums: vec![1],
                target_seqnum: Some(0),
            }),
            ..Default::default()
        }
        .encode_to_vec();

        // A raised hands message should be sent to all clients
        assert_eq!(
            vec![
                (
                    demux_id1,
                    create_server_to_client_rtp(
                        2,
                        &expected_update_payload_for_raised_hands_client1
                    )
                ),
                (
                    demux_id2,
                    create_server_to_client_rtp(
                        2,
                        &expected_update_payload_for_raised_hands_client2
                    )
                )
            ],
            rtp_to_send
        );
    }

    #[test]
    fn admin_messages() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_device_1 = add_admin(&mut call, "Alice", 1, at(100));
        let bob_device_1 = add_client(&mut call, "Bob", 2, at(200));
        assert_eq!(vec![bob_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        // Alice: Approve Bob
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_server_to_client_rtp(
                    1,
                    &protos::DeviceToSfu {
                        approve: vec![protos::device_to_sfu::GenericAdminAction {
                            target_demux_id: Some(bob_device_1.as_u32()),
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .borrow_mut(),
                at(300),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        // Alice: Remove Bob
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_server_to_client_rtp(
                    2,
                    &protos::DeviceToSfu {
                        remove: vec![protos::device_to_sfu::GenericAdminAction {
                            target_demux_id: Some(bob_device_1.as_u32()),
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .borrow_mut(),
                at(400),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![bob_device_1], demux_ids(&call.removed_clients));

        let carol_device_1 = add_client(&mut call, "Carol", 3, at(500));
        assert_eq!(vec![carol_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![bob_device_1], demux_ids(&call.removed_clients));

        // Alice: Deny Carol
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_server_to_client_rtp(
                    3,
                    &protos::DeviceToSfu {
                        deny: vec![protos::device_to_sfu::GenericAdminAction {
                            target_demux_id: Some(carol_device_1.as_u32()),
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .borrow_mut(),
                at(600),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(
            vec![bob_device_1, carol_device_1],
            demux_ids(&call.removed_clients)
        );

        let damien_device_1 = add_admin(&mut call, "Damien", 4, at(700));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(
            vec![alice_device_1, damien_device_1],
            demux_ids(&call.clients)
        );
        assert_eq!(
            vec![bob_device_1, carol_device_1],
            demux_ids(&call.removed_clients)
        );

        // Alice: Block Damien
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_server_to_client_rtp(
                    5,
                    &protos::DeviceToSfu {
                        block: vec![protos::device_to_sfu::GenericAdminAction {
                            target_demux_id: Some(damien_device_1.as_u32()),
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .borrow_mut(),
                at(800),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(
            vec![bob_device_1, carol_device_1, damien_device_1],
            demux_ids(&call.removed_clients)
        );
        assert_eq!(
            vec!["Damien"],
            call.blocked_users
                .iter()
                .map(UserId::as_str)
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn reliable_admin_messages() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);
        call.new_clients_require_approval = true;

        let alice_device_1 = add_admin(&mut call, "Alice", 1, at(100));
        let bob_device_1 = add_client(&mut call, "Bob", 2, at(200));
        assert_eq!(vec![bob_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.removed_clients));

        fn mrp_header_with_seqnum(seqnum: u64) -> Option<protos::MrpHeader> {
            Some(protos::MrpHeader {
                seqnum: Some(seqnum),
                ..Default::default()
            })
        }

        let first_approval = protos::DeviceToSfu {
            approve: vec![protos::device_to_sfu::GenericAdminAction {
                target_demux_id: Some(bob_device_1.as_u32()),
            }],
            mrp_header: mrp_header_with_seqnum(1),
            ..Default::default()
        }
        .encode_to_vec();
        let second_deny = protos::DeviceToSfu {
            deny: vec![protos::device_to_sfu::GenericAdminAction {
                target_demux_id: Some(bob_device_1.as_u32()),
            }],
            mrp_header: mrp_header_with_seqnum(2),
            ..Default::default()
        }
        .encode_to_vec();

        // Alice: Out of order deny, buffered and not processed, Bob still pending
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(2, &second_deny).borrow_mut(),
                at(100),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(
            vec![bob_device_1] as Vec<DemuxId>,
            demux_ids(&call.pending_clients)
        );
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        // Alice: Approve Bob, processes both the Approve then the Deny. Deny is then ignored
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(1, &first_approval).borrow_mut(),
                at(300),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        // Alice: Retransmits first approval, ignored, nothing changes
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(3, &first_approval).borrow_mut(),
                at(500),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        // Alice: Retransmits deny, ignored, nothing changes
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(4, &second_deny).borrow_mut(),
                at(500),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        // Carol: Joins
        let carol_device_1 = add_client(&mut call, "Carol", 3, at(500));
        let carol_user_id = UserId::from("Carol".to_string());
        assert_eq!(vec![carol_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        let third_deny = &protos::DeviceToSfu {
            deny: vec![protos::device_to_sfu::GenericAdminAction {
                target_demux_id: Some(carol_device_1.as_u32()),
            }],
            mrp_header: mrp_header_with_seqnum(3),
            ..Default::default()
        }
        .encode_to_vec();
        let fourth_approve = &protos::DeviceToSfu {
            approve: vec![protos::device_to_sfu::GenericAdminAction {
                target_demux_id: Some(carol_device_1.as_u32()),
            }],
            mrp_header: mrp_header_with_seqnum(4),
            ..Default::default()
        }
        .encode_to_vec();

        // Alice: Denies then Approves Carol. Received out of in order. Results in Carol being denied

        // Receive the approve first
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(6, fourth_approve).borrow_mut(),
                at(600),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());
        assert_eq!(vec![carol_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(call.removed_clients.is_empty());
        assert!(call.denied_users.is_empty());

        // then receive the deny - results in being denied
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(5, third_deny).borrow_mut(),
                at(600),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(carol_user_id == call.removed_clients[0].user_id);
        assert!(HashSet::from([carol_user_id.clone()]) == call.denied_users);

        // retransmitted deny - discarded, avoiding the accidental block
        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_reliable_server_to_client_rtp(5, third_deny).borrow_mut(),
                at(600),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());
        assert_eq!(vec![] as Vec<DemuxId>, demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1, bob_device_1], demux_ids(&call.clients));
        assert!(carol_user_id == call.removed_clients[0].user_id);
        assert!(HashSet::from([carol_user_id.clone()]) == call.denied_users);
    }

    #[test]
    fn non_admin_cannot_use_admin_messages() {
        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);

        let mut call = create_call(b"call_id", now, system_now);

        let alice_device_1 = add_client(&mut call, "Alice", 1, at(100));

        call.new_clients_require_approval = true;

        let bob_device_1 = add_client(&mut call, "Bob", 2, at(200));
        assert_eq!(vec![bob_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));

        let rtp_to_send = call
            .handle_rtp(
                alice_device_1,
                create_server_to_client_rtp(
                    1,
                    &protos::DeviceToSfu {
                        approve: vec![protos::device_to_sfu::GenericAdminAction {
                            target_demux_id: Some(bob_device_1.as_u32()),
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .borrow_mut(),
                at(300),
            )
            .unwrap();
        assert!(rtp_to_send.is_empty());

        // No change
        assert_eq!(vec![bob_device_1], demux_ids(&call.pending_clients));
        assert_eq!(vec![alice_device_1], demux_ids(&call.clients));
    }

    #[test]
    fn repeated_key_frame_requests() {
        let _ = env_logger::builder().is_test(true).try_init();

        let now = Instant::now();
        let system_now = SystemTime::now();
        let at = |millis| now + Duration::from_millis(millis);
        let get_forwarding_video_demux_ids =
            |from_server: &[RtpToSend], receiver_demux_id: DemuxId| -> Option<Vec<DemuxId>> {
                let (_demux_id, rtp) = from_server
                    .iter()
                    .find(|(demux_id, _rtp)| *demux_id == receiver_demux_id)?;
                let proto = protos::SfuToDevice::decode(rtp.payload()).ok()?;
                let mut demux_ids: Vec<DemuxId> = proto
                    .current_devices?
                    .demux_ids_with_video
                    .iter()
                    .map(|demux_id| DemuxId::try_from(*demux_id).unwrap())
                    .collect();
                demux_ids.sort();
                Some(demux_ids)
            };

        let mut call = create_call(b"call_id", now, system_now);
        let demux_id1 = add_client(&mut call, "1", 1, at(1));
        let demux_id2 = add_client(&mut call, "2", 2, at(2));
        let demux_id3 = add_client(&mut call, "3", 3, at(3));

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(4));
        // Nothing to forward yet
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );

        // Send some video from client2 and client3 so the incoming rate goes up.
        for seqnum in 0..10 {
            let mut to_server = create_video_rtp(
                demux_id2,
                LayerId::Video0,
                1,
                seqnum,
                Some(PixelSize {
                    width: 640,
                    height: 480,
                }),
            );
            call.handle_rtp(demux_id2, to_server.borrow_mut(), at(5))
                .unwrap();

            let mut to_server = create_video_rtp(
                demux_id3,
                LayerId::Video0,
                1,
                seqnum,
                Some(PixelSize {
                    width: 640,
                    height: 480,
                }),
            );
            call.handle_rtp(demux_id3, to_server.borrow_mut(), at(5))
                .unwrap();
        }

        let (from_server, _outgoing_key_frame_requests) = call.tick(at(1006));
        assert_eq!(
            Some(vec![demux_id2, demux_id3]),
            get_forwarding_video_demux_ids(&from_server, demux_id1)
        );
        assert_eq!(
            Some(vec![demux_id3]),
            get_forwarding_video_demux_ids(&from_server, demux_id2)
        );
        assert_eq!(
            Some(vec![demux_id2]),
            get_forwarding_video_demux_ids(&from_server, demux_id3)
        );

        // Initial key frame requests
        let mut outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(2000));
        outgoing_key_frame_requests.sort_unstable_by_key(|r| r.0);

        let expected_key_frame_requests = &[
            (
                demux_id2,
                rtp::KeyFrameRequest {
                    ssrc: LayerId::Video0.to_ssrc(demux_id2),
                },
            ),
            (
                demux_id3,
                rtp::KeyFrameRequest {
                    ssrc: LayerId::Video0.to_ssrc(demux_id3),
                },
            ),
        ];

        assert_eq!(outgoing_key_frame_requests, expected_key_frame_requests);

        // No repeat requests within 200ms.
        let outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(2100));
        assert_eq!(outgoing_key_frame_requests, &[]);

        // No change after.
        let mut outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(2200));
        outgoing_key_frame_requests.sort_unstable_by_key(|r| r.0);

        assert_eq!(outgoing_key_frame_requests, expected_key_frame_requests);

        // Send a keyframe for demux_id2 only.
        let mut to_server = create_video_rtp(
            demux_id2,
            LayerId::Video0,
            1,
            100,
            Some(PixelSize {
                width: 640,
                height: 480,
            }),
        );
        call.handle_rtp(demux_id2, to_server.borrow_mut(), at(2300))
            .unwrap();

        let outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(3000));

        assert_eq!(
            outgoing_key_frame_requests,
            &expected_key_frame_requests[1..]
        );

        // Re-request demux_id2 immediately after.
        // It's too soon for any new requests.
        let outgoing_key_frame_requests = call.handle_key_frame_requests(
            demux_id1,
            &[rtp::KeyFrameRequest {
                ssrc: LayerId::Video0.to_ssrc(demux_id2),
            }],
            at(3001),
        );
        assert_eq!(outgoing_key_frame_requests, &[]);

        // Even once we recompute requests, we've recently requested demux_id3.
        let outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(3100));
        assert_eq!(
            outgoing_key_frame_requests,
            &expected_key_frame_requests[..1]
        );

        // ...and now we've recently requested demux_id2.
        let outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(3200));
        assert_eq!(
            outgoing_key_frame_requests,
            &expected_key_frame_requests[1..]
        );

        // Only if we wait more than 200ms will we get both again.
        let mut outgoing_key_frame_requests =
            call.send_key_frame_requests_if_its_been_too_long(at(3500));
        outgoing_key_frame_requests.sort_unstable_by_key(|r| r.0);
        assert_eq!(outgoing_key_frame_requests, expected_key_frame_requests);
    }
}
