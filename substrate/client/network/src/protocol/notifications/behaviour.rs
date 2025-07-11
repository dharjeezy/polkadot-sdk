// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	protocol::notifications::{
		handler::{
			self, CloseReason, NotificationsSink, NotifsHandler, NotifsHandlerIn, NotifsHandlerOut,
		},
		service::{NotificationCommand, ProtocolHandle, ValidationCallResult},
	},
	protocol_controller::{self, IncomingIndex, Message, SetId},
	service::{
		metrics::NotificationMetrics,
		traits::{Direction, ValidationResult},
	},
	types::ProtocolName,
};

use bytes::BytesMut;
use fnv::FnvHashMap;
use futures::{future::BoxFuture, prelude::*, stream::FuturesUnordered};
use libp2p::{
	core::{transport::PortUse, Endpoint, Multiaddr},
	swarm::{
		behaviour::{ConnectionClosed, ConnectionEstablished, DialFailure, FromSwarm},
		ConnectionDenied, ConnectionId, DialError, NetworkBehaviour, NotifyHandler, THandler,
		THandlerInEvent, THandlerOutEvent, ToSwarm,
	},
	PeerId,
};
use log::{debug, error, trace, warn};
use parking_lot::RwLock;
use rand::distributions::{Distribution as _, Uniform};
use sc_utils::mpsc::TracingUnboundedReceiver;
use smallvec::SmallVec;
use tokio::sync::oneshot::error::RecvError;
use tokio_stream::StreamMap;

use libp2p::swarm::CloseConnection;
use std::{
	cmp,
	collections::{hash_map::Entry, VecDeque},
	mem,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
	time::{Duration, Instant},
};

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p::notification::behaviour";

/// Type representing a pending substream validation.
type PendingInboundValidation =
	BoxFuture<'static, (Result<ValidationResult, RecvError>, IncomingIndex)>;

/// Network behaviour that handles opening substreams for custom protocols with other peers.
///
/// # How it works
///
/// The role of the `Notifications` is to synchronize the following components:
///
/// - The libp2p swarm that opens new connections and reports disconnects.
/// - The connection handler (see `group.rs`) that handles individual connections.
/// - The peerset manager (PSM) that requests links to peers to be established or broken.
/// - The external API, that requires knowledge of the links that have been established.
///
/// In the state machine below, each `PeerId` is attributed one of these states:
///
/// - [`PeerState::Requested`]: No open connection, but requested by the peerset. Currently dialing.
/// - [`PeerState::Disabled`]: Has open TCP connection(s) unbeknownst to the peerset. No substream
///   is open.
/// - [`PeerState::Enabled`]: Has open TCP connection(s), acknowledged by the peerset.
///   - Notifications substreams are open on at least one connection, and external API has been
///     notified.
///   - Notifications substreams aren't open.
/// - [`PeerState::Incoming`]: Has open TCP connection(s) and remote would like to open substreams.
///   Peerset has been asked to attribute an inbound slot.
///
/// In addition to these states, there also exists a "banning" system. If we fail to dial a peer,
/// we back-off for a few seconds. If the PSM requests connecting to a peer that is currently
/// backed-off, the next dialing attempt is delayed until after the ban expires. However, the PSM
/// will still consider the peer to be connected. This "ban" is thus not a ban in a strict sense:
/// if a backed-off peer tries to connect, the connection is accepted. A ban only delays dialing
/// attempts.
///
/// There may be multiple connections to a peer. The status of a peer on
/// the API of this behaviour and towards the peerset manager is aggregated in
/// the following way:
///
///   1. The enabled/disabled status is the same across all connections, as decided by the peerset
///      manager.
///   2. `send_packet` and `write_notification` always send all data over the same connection to
///      preserve the ordering provided by the transport, as long as that connection is open. If it
///      closes, a second open connection may take over, if one exists, but that case should be no
///      different than a single connection failing and being re-established in terms of potential
///      reordering and dropped messages. Messages can be received on any connection.
///   3. The behaviour reports `NotificationsOut::CustomProtocolOpen` when the first connection
///      reports `NotifsHandlerOut::OpenResultOk`.
///   4. The behaviour reports `NotificationsOut::CustomProtocolClosed` when the last connection
///      reports `NotifsHandlerOut::ClosedResult`.
///
/// In this way, the number of actual established connections to the peer is
/// an implementation detail of this behaviour. Note that, in practice and at
/// the time of this writing, there may be at most two connections to a peer
/// and only as a result of simultaneous dialing. However, the implementation
/// accommodates for any number of connections.
pub struct Notifications {
	/// Notification protocols. Entries never change after initialization.
	notif_protocols: Vec<handler::ProtocolConfig>,

	/// Protocol handles.
	protocol_handles: Vec<ProtocolHandle>,

	// Command streams.
	command_streams: StreamMap<usize, Box<dyn Stream<Item = NotificationCommand> + Send + Unpin>>,

	/// Protocol controllers are responsible for peer connections management.
	protocol_controller_handles: Vec<protocol_controller::ProtocolHandle>,

	/// Receiver for instructions about who to connect to or disconnect from.
	from_protocol_controllers: TracingUnboundedReceiver<Message>,

	/// List of peers in our state.
	peers: FnvHashMap<(PeerId, SetId), PeerState>,

	/// The elements in `peers` occasionally contain `Delay` objects that we would normally have
	/// to be polled one by one. In order to avoid doing so, as an optimization, every `Delay` is
	/// instead put inside of `delays` and reference by a [`DelayId`]. This stream
	/// yields `PeerId`s whose `DelayId` is potentially ready.
	///
	/// By design, we never remove elements from this list. Elements are removed only when the
	/// `Delay` triggers. As such, this stream may produce obsolete elements.
	delays:
		stream::FuturesUnordered<Pin<Box<dyn Future<Output = (DelayId, PeerId, SetId)> + Send>>>,

	/// [`DelayId`] to assign to the next delay.
	next_delay_id: DelayId,

	/// List of incoming messages we have sent to the peer set manager and that are waiting for an
	/// answer.
	incoming: SmallVec<[IncomingPeer; 6]>,

	/// We generate indices to identify incoming connections. This is the next value for the index
	/// to use when a connection is incoming.
	next_incoming_index: IncomingIndex,

	/// Events to produce from `poll()`.
	events: VecDeque<ToSwarm<NotificationsOut, NotifsHandlerIn>>,

	/// Pending inbound substream validations.
	//
	// NOTE: it's possible to read a stale response from `pending_inbound_validations`
	// as the substream may get closed by the remote peer before the protocol has had
	// a chance to validate it. [`Notifications`] must compare the `crate::peerset::IncomingIndex`
	// returned by the completed future against the `crate::peerset::IncomingIndex` stored in
	// `PeerState::Incoming` to check whether the completed future is stale or not.
	pending_inbound_validations: FuturesUnordered<PendingInboundValidation>,

	/// Metrics for notifications.
	metrics: NotificationMetrics,
}

/// Configuration for a notifications protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
	/// Name of the protocol.
	pub name: ProtocolName,
	/// Names of the protocol to use if the main one isn't available.
	pub fallback_names: Vec<ProtocolName>,
	/// Handshake of the protocol.
	pub handshake: Vec<u8>,
	/// Maximum allowed size for a notification.
	pub max_notification_size: u64,
}

/// Identifier for a delay firing.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct DelayId(u64);

/// State of a peer we're connected to.
///
/// The variants correspond to the state of the peer w.r.t. the peerset.
#[derive(Debug)]
enum PeerState {
	/// State is poisoned. This is a temporary state for a peer and we should always switch back
	/// to it later. If it is found in the wild, that means there was either a panic or a bug in
	/// the state machine code.
	Poisoned,

	/// The peer misbehaved. If the PSM wants us to connect to this peer, we will add an artificial
	/// delay to the connection.
	Backoff {
		/// When the ban expires. For clean-up purposes. References an entry in `delays`.
		timer: DelayId,
		/// Until when the peer is backed-off.
		timer_deadline: Instant,
	},

	/// The peerset requested that we connect to this peer. We are currently not connected.
	PendingRequest {
		/// When to actually start dialing. References an entry in `delays`.
		timer: DelayId,
		/// When the `timer` will trigger.
		timer_deadline: Instant,
	},

	/// The peerset requested that we connect to this peer. We are currently dialing this peer.
	Requested,

	/// We are connected to this peer but the peerset hasn't requested it or has denied it.
	///
	/// The handler is either in the closed state, or a `Close` message has been sent to it and
	/// hasn't been answered yet.
	Disabled {
		/// If `Some`, any connection request from the peerset to this peer is delayed until the
		/// given `Instant`.
		backoff_until: Option<Instant>,

		/// List of connections with this peer, and their state.
		connections: SmallVec<[(ConnectionId, ConnectionState); crate::MAX_CONNECTIONS_PER_PEER]>,
	},

	/// We are connected to this peer. The peerset has requested a connection to this peer, but
	/// it is currently in a "backed-off" phase. The state will switch to `Enabled` once the timer
	/// expires.
	///
	/// The handler is either in the closed state, or a `Close` message has been sent to it and
	/// hasn't been answered yet.
	///
	/// The handler will be opened when `timer` fires.
	DisabledPendingEnable {
		/// When to enable this remote. References an entry in `delays`.
		timer: DelayId,
		/// When the `timer` will trigger.
		timer_deadline: Instant,

		/// List of connections with this peer, and their state.
		connections: SmallVec<[(ConnectionId, ConnectionState); crate::MAX_CONNECTIONS_PER_PEER]>,
	},

	/// We are connected to this peer and the peerset has accepted it.
	Enabled {
		/// List of connections with this peer, and their state.
		connections: SmallVec<[(ConnectionId, ConnectionState); crate::MAX_CONNECTIONS_PER_PEER]>,
	},

	/// We are connected to this peer. We have received an `OpenDesiredByRemote` from one of the
	/// handlers and forwarded that request to the peerset. The connection handlers are waiting for
	/// a response, i.e. to be opened or closed based on whether the peerset accepts or rejects
	/// the peer.
	Incoming {
		/// If `Some`, any dial attempts to this peer are delayed until the given `Instant`.
		backoff_until: Option<Instant>,

		/// Incoming index tracking this connection.
		incoming_index: IncomingIndex,

		/// Peerset has signaled it wants the substream closed.
		peerset_rejected: bool,

		/// List of connections with this peer, and their state.
		connections: SmallVec<[(ConnectionId, ConnectionState); crate::MAX_CONNECTIONS_PER_PEER]>,
	},
}

impl PeerState {
	/// True if there exists an established connection to the peer
	/// that is open for custom protocol traffic.
	fn is_open(&self) -> bool {
		self.get_open().is_some()
	}

	/// Returns the [`NotificationsSink`] of the first established connection
	/// that is open for custom protocol traffic.
	fn get_open(&self) -> Option<&NotificationsSink> {
		match self {
			Self::Enabled { connections, .. } => connections.iter().find_map(|(_, s)| match s {
				ConnectionState::Open(s) => Some(s),
				_ => None,
			}),
			_ => None,
		}
	}
}

/// State of the handler of a single connection visible from this state machine.
#[derive(Debug)]
enum ConnectionState {
	/// Connection is in the `Closed` state, meaning that the remote hasn't requested anything.
	Closed,

	/// Connection is either in the `Open` or the `Closed` state, but a
	/// [`NotifsHandlerIn::Close`] message has been sent. Waiting for this message to be
	/// acknowledged through a [`NotifsHandlerOut::CloseResult`].
	Closing,

	/// Connection is in the `Closed` state but a [`NotifsHandlerIn::Open`] message has been sent.
	/// An `OpenResultOk`/`OpenResultErr` message is expected.
	Opening,

	/// Connection is in the `Closed` state but a [`NotifsHandlerIn::Open`] message then a
	/// [`NotifsHandlerIn::Close`] message has been sent. An `OpenResultOk`/`OpenResultErr` message
	/// followed with a `CloseResult` message are expected.
	OpeningThenClosing,

	/// Connection is in the `Closed` state, but a [`NotifsHandlerOut::OpenDesiredByRemote`]
	/// message has been received, meaning that the remote wants to open a substream.
	OpenDesiredByRemote,

	/// Connection is in the `Open` state.
	///
	/// The external API is notified of a channel with this peer if any of its connection is in
	/// this state.
	Open(NotificationsSink),
}

/// State of an "incoming" message sent to the peer set manager.
#[derive(Debug)]
struct IncomingPeer {
	/// Id of the remote peer of the incoming substream.
	peer_id: PeerId,
	/// Id of the set the incoming substream would belong to.
	set_id: SetId,
	/// If true, this "incoming" still corresponds to an actual connection. If false, then the
	/// connection corresponding to it has been closed or replaced already.
	alive: bool,
	/// Id that the we sent to the peerset.
	incoming_id: IncomingIndex,
	/// Received handshake.
	handshake: Vec<u8>,
}

/// Event that can be emitted by the `Notifications`.
#[derive(Debug)]
pub enum NotificationsOut {
	/// Opened a custom protocol with the remote.
	CustomProtocolOpen {
		/// Id of the peer we are connected to.
		peer_id: PeerId,
		/// Peerset set ID the substream is tied to.
		set_id: SetId,
		/// Direction of the stream.
		direction: Direction,
		/// If `Some`, a fallback protocol name has been used rather the main protocol name.
		/// Always matches one of the fallback names passed at initialization.
		negotiated_fallback: Option<ProtocolName>,
		/// Handshake that was sent to us.
		/// This is normally a "Status" message, but this is out of the concern of this code.
		received_handshake: Vec<u8>,
		/// Object that permits sending notifications to the peer.
		notifications_sink: NotificationsSink,
	},

	/// The [`NotificationsSink`] object used to send notifications with the given peer must be
	/// replaced with a new one.
	///
	/// This event is typically emitted when a transport-level connection is closed and we fall
	/// back to a secondary connection.
	CustomProtocolReplaced {
		/// Id of the peer we are connected to.
		peer_id: PeerId,
		/// Peerset set ID the substream is tied to.
		set_id: SetId,
		/// Replacement for the previous [`NotificationsSink`].
		notifications_sink: NotificationsSink,
	},

	/// Closed a custom protocol with the remote. The existing [`NotificationsSink`] should
	/// be dropped.
	CustomProtocolClosed {
		/// Id of the peer we were connected to.
		peer_id: PeerId,
		/// Peerset set ID the substream was tied to.
		set_id: SetId,
	},

	/// Receives a message on a custom protocol substream.
	///
	/// Also concerns received notifications for the notifications API.
	Notification {
		/// Id of the peer the message came from.
		peer_id: PeerId,
		/// Peerset set ID the substream is tied to.
		set_id: SetId,
		/// Message that has been received.
		message: BytesMut,
	},

	/// The remote peer misbehaved by sent a message on an outbound substream.
	ProtocolMisbehavior {
		/// Id of the peer the message came from.
		peer_id: PeerId,
		/// Peerset set ID the substream is tied to.
		set_id: SetId,
	},
}

impl Notifications {
	/// Creates a `CustomProtos`.
	pub(crate) fn new(
		protocol_controller_handles: Vec<protocol_controller::ProtocolHandle>,
		from_protocol_controllers: TracingUnboundedReceiver<Message>,
		metrics: NotificationMetrics,
		notif_protocols: impl Iterator<
			Item = (
				ProtocolConfig,
				ProtocolHandle,
				Box<dyn Stream<Item = NotificationCommand> + Send + Unpin>,
			),
		>,
	) -> Self {
		let (notif_protocols, protocol_handles): (Vec<_>, Vec<_>) = notif_protocols
			.map(|(cfg, protocol_handle, command_stream)| {
				(
					handler::ProtocolConfig {
						name: cfg.name,
						fallback_names: cfg.fallback_names,
						handshake: Arc::new(RwLock::new(cfg.handshake)),
						max_notification_size: cfg.max_notification_size,
					},
					(protocol_handle, command_stream),
				)
			})
			.unzip();
		assert!(!notif_protocols.is_empty());

		let (mut protocol_handles, command_streams): (Vec<_>, Vec<_>) = protocol_handles
			.into_iter()
			.enumerate()
			.map(|(set_id, (mut protocol_handle, command_stream))| {
				protocol_handle.set_metrics(metrics.clone());

				(protocol_handle, (set_id, command_stream))
			})
			.unzip();

		protocol_handles.iter_mut().skip(1).for_each(|handle| {
			handle.delegate_to_peerset(true);
		});

		Self {
			notif_protocols,
			protocol_handles,
			command_streams: StreamMap::from_iter(command_streams.into_iter()),
			protocol_controller_handles,
			from_protocol_controllers,
			peers: FnvHashMap::default(),
			delays: Default::default(),
			next_delay_id: DelayId(0),
			incoming: SmallVec::new(),
			next_incoming_index: IncomingIndex(0),
			events: VecDeque::new(),
			pending_inbound_validations: FuturesUnordered::new(),
			metrics,
		}
	}

	/// Modifies the handshake of the given notifications protocol.
	pub fn set_notif_protocol_handshake(
		&mut self,
		set_id: SetId,
		handshake_message: impl Into<Vec<u8>>,
	) {
		if let Some(p) = self.notif_protocols.get_mut(usize::from(set_id)) {
			*p.handshake.write() = handshake_message.into();
		} else {
			log::error!(target: LOG_TARGET, "Unknown handshake change set: {:?}", set_id);
			debug_assert!(false);
		}
	}

	/// Returns the list of all the peers we have an open channel to.
	pub fn open_peers(&self) -> impl Iterator<Item = &PeerId> {
		self.peers.iter().filter(|(_, state)| state.is_open()).map(|((id, _), _)| id)
	}

	/// Returns true if we have an open substream to the given peer.
	pub fn is_open(&self, peer_id: &PeerId, set_id: SetId) -> bool {
		self.peers.get(&(*peer_id, set_id)).map(|p| p.is_open()).unwrap_or(false)
	}

	/// Disconnects the given peer if we are connected to it.
	pub fn disconnect_peer(&mut self, peer_id: &PeerId, set_id: SetId) {
		trace!(target: LOG_TARGET, "External API => Disconnect({}, {:?})", peer_id, set_id);
		self.disconnect_peer_inner(peer_id, set_id);
	}

	/// Inner implementation of `disconnect_peer`.
	fn disconnect_peer_inner(&mut self, peer_id: &PeerId, set_id: SetId) {
		let mut entry = if let Entry::Occupied(entry) = self.peers.entry((*peer_id, set_id)) {
			entry
		} else {
			return
		};

		match mem::replace(entry.get_mut(), PeerState::Poisoned) {
			// We're not connected anyway.
			st @ PeerState::Disabled { .. } => *entry.into_mut() = st,
			st @ PeerState::Requested => *entry.into_mut() = st,
			st @ PeerState::PendingRequest { .. } => *entry.into_mut() = st,
			st @ PeerState::Backoff { .. } => *entry.into_mut() = st,

			// DisabledPendingEnable => Disabled.
			PeerState::DisabledPendingEnable { connections, timer_deadline, timer: _ } => {
				trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
				self.protocol_controller_handles[usize::from(set_id)].dropped(*peer_id);
				*entry.into_mut() =
					PeerState::Disabled { connections, backoff_until: Some(timer_deadline) }
			},

			// Enabled => Disabled.
			// All open or opening connections are sent a `Close` message.
			// If relevant, the external API is instantly notified.
			PeerState::Enabled { mut connections } => {
				trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
				self.protocol_controller_handles[usize::from(set_id)].dropped(*peer_id);

				if connections.iter().any(|(_, s)| matches!(s, ConnectionState::Open(_))) {
					trace!(target: LOG_TARGET, "External API <= Closed({}, {:?})", peer_id, set_id);
					let event =
						NotificationsOut::CustomProtocolClosed { peer_id: *peer_id, set_id };
					self.events.push_back(ToSwarm::GenerateEvent(event));
				}

				for (connec_id, connec_state) in
					connections.iter_mut().filter(|(_, s)| matches!(s, ConnectionState::Open(_)))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})", peer_id, *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: *peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
					});
					*connec_state = ConnectionState::Closing;
				}

				for (connec_id, connec_state) in
					connections.iter_mut().filter(|(_, s)| matches!(s, ConnectionState::Opening))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})", peer_id, *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: *peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
					});
					*connec_state = ConnectionState::OpeningThenClosing;
				}

				debug_assert!(!connections
					.iter()
					.any(|(_, s)| matches!(s, ConnectionState::Open(_))));
				debug_assert!(!connections
					.iter()
					.any(|(_, s)| matches!(s, ConnectionState::Opening)));

				*entry.into_mut() = PeerState::Disabled { connections, backoff_until: None }
			},

			// Incoming => Disabled.
			// Ongoing opening requests from the remote are rejected.
			PeerState::Incoming { mut connections, backoff_until, .. } => {
				let inc = if let Some(inc) = self
					.incoming
					.iter_mut()
					.find(|i| i.peer_id == entry.key().0 && i.set_id == set_id && i.alive)
				{
					inc
				} else {
					error!(
						target: LOG_TARGET,
						"State mismatch in libp2p: no entry in incoming for incoming peer"
					);
					return
				};

				inc.alive = false;

				for (connec_id, connec_state) in connections
					.iter_mut()
					.filter(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})", peer_id, *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: *peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
					});
					*connec_state = ConnectionState::Closing;
				}

				debug_assert!(!connections
					.iter()
					.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote)));
				*entry.into_mut() = PeerState::Disabled { connections, backoff_until }
			},

			PeerState::Poisoned => {
				error!(target: LOG_TARGET, "State of {:?} is poisoned", peer_id)
			},
		}
	}

	/// Function that is called when the peerset wants us to connect to a peer.
	fn peerset_report_connect(&mut self, peer_id: PeerId, set_id: SetId) {
		// If `PeerId` is unknown to us, insert an entry, start dialing, and return early.
		let mut occ_entry = match self.peers.entry((peer_id, set_id)) {
			Entry::Occupied(entry) => entry,
			Entry::Vacant(entry) => {
				// If there's no entry in `self.peers`, start dialing.
				trace!(
					target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Starting to connect",
					entry.key().0,
					set_id,
				);
				trace!(target: LOG_TARGET, "Libp2p <= Dial {}", entry.key().0);
				self.events.push_back(ToSwarm::Dial { opts: entry.key().0.into() });
				entry.insert(PeerState::Requested);
				return
			},
		};

		let now = Instant::now();

		match mem::replace(occ_entry.get_mut(), PeerState::Poisoned) {
			// Backoff (not expired) => PendingRequest
			PeerState::Backoff { ref timer, ref timer_deadline } if *timer_deadline > now => {
				let peer_id = occ_entry.key().0;
				trace!(
					target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Will start to connect at {:?}",
					peer_id,
					set_id,
					timer_deadline,
				);
				*occ_entry.into_mut() =
					PeerState::PendingRequest { timer: *timer, timer_deadline: *timer_deadline };
			},

			// Backoff (expired) => Requested
			PeerState::Backoff { .. } => {
				trace!(
					target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Starting to connect",
					occ_entry.key().0,
					set_id,
				);
				trace!(target: LOG_TARGET, "Libp2p <= Dial {:?}", occ_entry.key());
				self.events.push_back(ToSwarm::Dial { opts: occ_entry.key().0.into() });
				*occ_entry.into_mut() = PeerState::Requested;
			},

			// Disabled (with non-expired ban) => DisabledPendingEnable
			PeerState::Disabled { connections, backoff_until: Some(ref backoff) }
				if *backoff > now =>
			{
				let peer_id = occ_entry.key().0;
				trace!(
					target: LOG_TARGET,
					"PSM => Connect({}, {:?}): But peer is backed-off until {:?}",
					peer_id,
					set_id,
					backoff,
				);

				let delay_id = self.next_delay_id;
				self.next_delay_id.0 += 1;
				let delay = futures_timer::Delay::new(*backoff - now);
				self.delays.push(
					async move {
						delay.await;
						(delay_id, peer_id, set_id)
					}
					.boxed(),
				);

				*occ_entry.into_mut() = PeerState::DisabledPendingEnable {
					connections,
					timer: delay_id,
					timer_deadline: *backoff,
				};
			},

			// Disabled => Enabled
			PeerState::Disabled { mut connections, backoff_until } => {
				debug_assert!(!connections
					.iter()
					.any(|(_, s)| { matches!(s, ConnectionState::Open(_)) }));

				// The first element of `closed` is chosen to open the notifications substream.
				if let Some((connec_id, connec_state)) =
					connections.iter_mut().find(|(_, s)| matches!(s, ConnectionState::Closed))
				{
					trace!(target: LOG_TARGET, "PSM => Connect({}, {:?}): Enabling connections.",
						occ_entry.key().0, set_id);
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Open({:?})", peer_id, *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Open { protocol_index: set_id.into(), peer_id },
					});
					*connec_state = ConnectionState::Opening;
					*occ_entry.into_mut() = PeerState::Enabled { connections };
				} else {
					// If no connection is available, switch to `DisabledPendingEnable` in order
					// to try again later.
					debug_assert!(connections.iter().any(|(_, s)| {
						matches!(s, ConnectionState::OpeningThenClosing | ConnectionState::Closing)
					}));
					trace!(
						target: LOG_TARGET,
						"PSM => Connect({}, {:?}): No connection in proper state. Delaying.",
						occ_entry.key().0, set_id
					);

					let timer_deadline = {
						let base = now + Duration::from_secs(5);
						if let Some(backoff_until) = backoff_until {
							cmp::max(base, backoff_until)
						} else {
							base
						}
					};

					let delay_id = self.next_delay_id;
					self.next_delay_id.0 += 1;
					debug_assert!(timer_deadline > now);
					let delay = futures_timer::Delay::new(timer_deadline - now);
					self.delays.push(
						async move {
							delay.await;
							(delay_id, peer_id, set_id)
						}
						.boxed(),
					);

					*occ_entry.into_mut() = PeerState::DisabledPendingEnable {
						connections,
						timer: delay_id,
						timer_deadline,
					};
				}
			},
			// Incoming => Incoming
			st @ PeerState::Incoming { .. } => {
				debug!(
					target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Ignoring obsolete connect, we are awaiting accept/reject.",
					occ_entry.key().0, set_id
				);
				*occ_entry.into_mut() = st;
			},

			// Other states are kept as-is.
			st @ PeerState::Enabled { .. } => {
				debug!(target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Already connected.",
					occ_entry.key().0, set_id);
				*occ_entry.into_mut() = st;
			},
			st @ PeerState::DisabledPendingEnable { .. } => {
				debug!(target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Already pending enabling.",
					occ_entry.key().0, set_id);
				*occ_entry.into_mut() = st;
			},
			st @ PeerState::Requested { .. } | st @ PeerState::PendingRequest { .. } => {
				debug!(target: LOG_TARGET,
					"PSM => Connect({}, {:?}): Duplicate request.",
					occ_entry.key().0, set_id);
				*occ_entry.into_mut() = st;
			},

			PeerState::Poisoned => {
				error!(target: LOG_TARGET, "State of {:?} is poisoned", occ_entry.key());
				debug_assert!(false);
			},
		}
	}

	/// Function that is called when the peerset wants us to disconnect from a peer.
	fn peerset_report_disconnect(&mut self, peer_id: PeerId, set_id: SetId) {
		let mut entry = match self.peers.entry((peer_id, set_id)) {
			Entry::Occupied(entry) => entry,
			Entry::Vacant(entry) => {
				trace!(target: LOG_TARGET, "PSM => Drop({}, {:?}): Already disabled.",
					entry.key().0, set_id);
				return
			},
		};

		match mem::replace(entry.get_mut(), PeerState::Poisoned) {
			st @ PeerState::Disabled { .. } | st @ PeerState::Backoff { .. } => {
				trace!(target: LOG_TARGET, "PSM => Drop({}, {:?}): Already disabled.",
					entry.key().0, set_id);
				*entry.into_mut() = st;
			},

			// DisabledPendingEnable => Disabled
			PeerState::DisabledPendingEnable { connections, timer_deadline, timer: _ } => {
				debug_assert!(!connections.is_empty());
				trace!(target: LOG_TARGET,
					"PSM => Drop({}, {:?}): Interrupting pending enabling.",
					entry.key().0, set_id);
				*entry.into_mut() =
					PeerState::Disabled { connections, backoff_until: Some(timer_deadline) };
			},

			// Enabled => Disabled
			PeerState::Enabled { mut connections } => {
				trace!(target: LOG_TARGET, "PSM => Drop({}, {:?}): Disabling connections.",
					entry.key().0, set_id);

				debug_assert!(connections.iter().any(|(_, s)| matches!(
					s,
					ConnectionState::Opening | ConnectionState::Open(_)
				)));

				if connections.iter().any(|(_, s)| matches!(s, ConnectionState::Open(_))) {
					trace!(target: LOG_TARGET, "External API <= Closed({}, {:?})", entry.key().0, set_id);
					let event =
						NotificationsOut::CustomProtocolClosed { peer_id: entry.key().0, set_id };
					self.events.push_back(ToSwarm::GenerateEvent(event));
				}

				for (connec_id, connec_state) in
					connections.iter_mut().filter(|(_, s)| matches!(s, ConnectionState::Opening))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})",
						entry.key(), *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: entry.key().0,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
					});
					*connec_state = ConnectionState::OpeningThenClosing;
				}

				for (connec_id, connec_state) in
					connections.iter_mut().filter(|(_, s)| matches!(s, ConnectionState::Open(_)))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})",
						entry.key(), *connec_id, set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: entry.key().0,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
					});
					*connec_state = ConnectionState::Closing;
				}

				*entry.into_mut() = PeerState::Disabled { connections, backoff_until: None }
			},

			// Requested => Ø
			PeerState::Requested => {
				// We don't cancel dialing. Libp2p doesn't expose that on purpose, as other
				// sub-systems (such as the discovery mechanism) may require dialing this peer as
				// well at the same time.
				trace!(target: LOG_TARGET, "PSM => Drop({}, {:?}): Not yet connected.",
					entry.key().0, set_id);
				entry.remove();
			},

			// PendingRequest => Backoff
			PeerState::PendingRequest { timer, timer_deadline } => {
				trace!(target: LOG_TARGET, "PSM => Drop({}, {:?}): Not yet connected",
					entry.key().0, set_id);
				*entry.into_mut() = PeerState::Backoff { timer, timer_deadline }
			},

			// `ProtocolController` disconnected peer while it was still being validated by the
			// protocol, mark the connection as rejected and once the validation is received from
			// the protocol, reject the substream
			PeerState::Incoming { backoff_until, connections, incoming_index, .. } => {
				debug!(
					target: LOG_TARGET,
					"PSM => Drop({}, {:?}): Ignoring obsolete disconnect, we are awaiting accept/reject.",
					entry.key().0, set_id,
				);
				*entry.into_mut() = PeerState::Incoming {
					backoff_until,
					connections,
					incoming_index,
					peerset_rejected: true,
				};
			},
			PeerState::Poisoned => {
				error!(target: LOG_TARGET, "State of {:?} is poisoned", entry.key());
				debug_assert!(false);
			},
		}
	}

	/// Substream has been accepted by the `ProtocolController` and must now be sent
	/// to the protocol for validation.
	fn peerset_report_preaccept(&mut self, index: IncomingIndex) {
		let Some(pos) = self.incoming.iter().position(|i| i.incoming_id == index) else {
			error!(target: LOG_TARGET, "PSM => Preaccept({:?}): Invalid index", index);
			return
		};

		trace!(
			target: LOG_TARGET,
			"PSM => Preaccept({:?}): Sent to protocol for validation",
			index
		);
		let incoming = &self.incoming[pos];

		match self.protocol_handles[usize::from(incoming.set_id)]
			.report_incoming_substream(incoming.peer_id, incoming.handshake.clone())
		{
			Ok(ValidationCallResult::Delegated) => {
				self.protocol_report_accept(index);
			},
			Ok(ValidationCallResult::WaitForValidation(rx)) => {
				self.pending_inbound_validations
					.push(Box::pin(async move { (rx.await, index) }));
			},
			Err(err) => {
				// parachain collators enable the syncing protocol but `NotificationService` for
				// `SyncingEngine` is not created which causes `report_incoming_substream()` to
				// fail. This is not a fatal error and should be ignored even though in typical
				// cases the `NotificationService` not existing is a fatal error and indicates that
				// the protocol has exited. Until the parachain collator issue is fixed, just report
				// and error and reject the peer.
				debug!(target: LOG_TARGET, "protocol has exited: {err:?} {:?}", incoming.set_id);

				self.protocol_report_reject(index);
			},
		}
	}

	/// Function that is called when the peerset wants us to accept a connection
	/// request from a peer.
	fn protocol_report_accept(&mut self, index: IncomingIndex) {
		let (pos, incoming) =
			if let Some(pos) = self.incoming.iter().position(|i| i.incoming_id == index) {
				(pos, self.incoming.get(pos))
			} else {
				error!(target: LOG_TARGET, "PSM => Accept({:?}): Invalid index", index);
				return
			};

		let Some(incoming) = incoming else {
			error!(target: LOG_TARGET, "Incoming connection ({:?}) doesn't exist", index);
			debug_assert!(false);
			return;
		};

		if !incoming.alive {
			trace!(
				target: LOG_TARGET,
				"PSM => Accept({:?}, {}, {:?}): Obsolete incoming",
				index,
				incoming.peer_id,
				incoming.set_id,
			);

			match self.peers.get_mut(&(incoming.peer_id, incoming.set_id)) {
				Some(PeerState::DisabledPendingEnable { .. }) | Some(PeerState::Enabled { .. }) => {
				},
				_ => {
					trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})",
						incoming.peer_id, incoming.set_id);
					self.protocol_controller_handles[usize::from(incoming.set_id)]
						.dropped(incoming.peer_id);
				},
			}

			self.incoming.remove(pos);
			return
		}

		let state = match self.peers.get_mut(&(incoming.peer_id, incoming.set_id)) {
			Some(s) => s,
			None => {
				log::debug!(
					target: LOG_TARGET,
					"Connection to {:?} closed, ({:?} {:?}), ignoring accept",
					incoming.peer_id,
					incoming.set_id,
					index,
				);
				self.incoming.remove(pos);
				return
			},
		};

		match mem::replace(state, PeerState::Poisoned) {
			// Incoming => Enabled
			PeerState::Incoming {
				mut connections,
				incoming_index,
				peerset_rejected,
				backoff_until,
			} => {
				if index < incoming_index {
					warn!(
						target: LOG_TARGET,
						"PSM => Accept({:?}, {}, {:?}): Ignoring obsolete incoming index, we are already awaiting {:?}.",
						index, incoming.peer_id, incoming.set_id, incoming_index
					);

					self.incoming.remove(pos);
					return
				} else if index > incoming_index {
					error!(
						target: LOG_TARGET,
						"PSM => Accept({:?}, {}, {:?}): Ignoring incoming index from the future, we are awaiting {:?}.",
						index, incoming.peer_id, incoming.set_id, incoming_index
					);

					self.incoming.remove(pos);
					debug_assert!(false);
					return
				}

				// while the substream was being validated by the protocol, `Peerset` had request
				// for the it to be closed so reject the substream now
				if peerset_rejected {
					trace!(
						target: LOG_TARGET,
						"Protocol accepted ({:?} {:?} {:?}) but Peerset had requested disconnection, rejecting",
						index,
						incoming.peer_id,
						incoming.set_id
					);

					*state = PeerState::Incoming {
						connections,
						backoff_until,
						peerset_rejected,
						incoming_index,
					};
					return self.report_reject(index).map_or((), |_| ())
				}

				trace!(
					target: LOG_TARGET,
					"PSM => Accept({:?}, {}, {:?}): Enabling connections.",
					index,
					incoming.peer_id,
					incoming.set_id
				);

				debug_assert!(connections
					.iter()
					.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote)));
				for (connec_id, connec_state) in connections
					.iter_mut()
					.filter(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Open({:?})",
						incoming.peer_id, *connec_id, incoming.set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: incoming.peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Open {
							protocol_index: incoming.set_id.into(),
							peer_id: incoming.peer_id,
						},
					});
					*connec_state = ConnectionState::Opening;
				}

				self.incoming.remove(pos);
				*state = PeerState::Enabled { connections };
			},
			st @ PeerState::Disabled { .. } | st @ PeerState::Backoff { .. } => {
				self.incoming.remove(pos);
				*state = st;
			},
			// Any state other than `Incoming` is invalid.
			peer => {
				error!(
					target: LOG_TARGET,
					"State mismatch in libp2p: Expected alive incoming. Got {:?}.",
					peer
				);

				self.incoming.remove(pos);
				debug_assert!(false);
			},
		}
	}

	/// Function that is called when `ProtocolController` wants us to reject an incoming peer.
	fn peerset_report_reject(&mut self, index: IncomingIndex) {
		let _ = self.report_reject(index);
	}

	/// Function that is called when the protocol wants us to reject an incoming peer.
	fn protocol_report_reject(&mut self, index: IncomingIndex) {
		if let Some((set_id, peer_id)) = self.report_reject(index) {
			self.protocol_controller_handles[usize::from(set_id)].dropped(peer_id)
		}
	}

	/// Function that is called when the peerset wants us to reject an incoming peer.
	fn report_reject(&mut self, index: IncomingIndex) -> Option<(SetId, PeerId)> {
		let incoming = if let Some(pos) = self.incoming.iter().position(|i| i.incoming_id == index)
		{
			self.incoming.remove(pos)
		} else {
			error!(target: LOG_TARGET, "PSM => Reject({:?}): Invalid index", index);
			return None
		};

		if !incoming.alive {
			trace!(
				target: LOG_TARGET,
				"PSM => Reject({:?}, {}, {:?}): Obsolete incoming, ignoring",
				index,
				incoming.peer_id,
				incoming.set_id,
			);

			return None
		}

		let state = match self.peers.get_mut(&(incoming.peer_id, incoming.set_id)) {
			Some(s) => s,
			None => {
				log::debug!(
					target: LOG_TARGET,
					"Connection to {:?} closed, ({:?} {:?}), ignoring accept",
					incoming.peer_id,
					incoming.set_id,
					index,
				);
				return None
			},
		};

		match mem::replace(state, PeerState::Poisoned) {
			// Incoming => Disabled
			PeerState::Incoming { mut connections, backoff_until, incoming_index, .. } => {
				if index < incoming_index {
					warn!(
						target: LOG_TARGET,
						"PSM => Reject({:?}, {}, {:?}): Ignoring obsolete incoming index, we are already awaiting {:?}.",
						index, incoming.peer_id, incoming.set_id, incoming_index
					);
					return None
				} else if index > incoming_index {
					error!(
						target: LOG_TARGET,
						"PSM => Reject({:?}, {}, {:?}): Ignoring incoming index from the future, we are awaiting {:?}.",
						index, incoming.peer_id, incoming.set_id, incoming_index
					);
					debug_assert!(false);
					return None
				}

				trace!(target: LOG_TARGET, "PSM => Reject({:?}, {}, {:?}): Rejecting connections.",
					index, incoming.peer_id, incoming.set_id);

				debug_assert!(connections
					.iter()
					.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote)));
				for (connec_id, connec_state) in connections
					.iter_mut()
					.filter(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote))
				{
					trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Close({:?})",
						incoming.peer_id, connec_id, incoming.set_id);
					self.events.push_back(ToSwarm::NotifyHandler {
						peer_id: incoming.peer_id,
						handler: NotifyHandler::One(*connec_id),
						event: NotifsHandlerIn::Close { protocol_index: incoming.set_id.into() },
					});
					*connec_state = ConnectionState::Closing;
				}

				*state = PeerState::Disabled { connections, backoff_until };
				Some((incoming.set_id, incoming.peer_id))
			},
			// connection to peer may have been closed already
			st @ PeerState::Disabled { .. } | st @ PeerState::Backoff { .. } => {
				*state = st;
				None
			},
			peer => {
				error!(
					target: LOG_TARGET,
					"State mismatch in libp2p: Expected alive incoming. Got {peer:?}.",
				);
				None
			},
		}
	}
}

impl NetworkBehaviour for Notifications {
	type ConnectionHandler = NotifsHandler;
	type ToSwarm = NotificationsOut;

	fn handle_pending_inbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		_local_addr: &Multiaddr,
		_remote_addr: &Multiaddr,
	) -> Result<(), ConnectionDenied> {
		Ok(())
	}

	fn handle_pending_outbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		_maybe_peer: Option<PeerId>,
		_addresses: &[Multiaddr],
		_effective_role: Endpoint,
	) -> Result<Vec<Multiaddr>, ConnectionDenied> {
		Ok(Vec::new())
	}

	fn handle_established_inbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		peer: PeerId,
		_local_addr: &Multiaddr,
		_remote_addr: &Multiaddr,
	) -> Result<THandler<Self>, ConnectionDenied> {
		Ok(NotifsHandler::new(peer, self.notif_protocols.clone(), Some(self.metrics.clone())))
	}

	fn handle_established_outbound_connection(
		&mut self,
		_connection_id: ConnectionId,
		peer: PeerId,
		_addr: &Multiaddr,
		_role_override: Endpoint,
		_port_use: PortUse,
	) -> Result<THandler<Self>, ConnectionDenied> {
		Ok(NotifsHandler::new(peer, self.notif_protocols.clone(), Some(self.metrics.clone())))
	}

	fn on_swarm_event(&mut self, event: FromSwarm) {
		match event {
			FromSwarm::ConnectionEstablished(ConnectionEstablished {
				peer_id,
				endpoint,
				connection_id,
				..
			}) => {
				for set_id in (0..self.notif_protocols.len()).map(SetId::from) {
					match self.peers.entry((peer_id, set_id)).or_insert(PeerState::Poisoned) {
						// Requested | PendingRequest => Enabled
						st @ &mut PeerState::Requested |
						st @ &mut PeerState::PendingRequest { .. } => {
							trace!(target: LOG_TARGET,
								"Libp2p => Connected({}, {:?}, {:?}): Connection was requested by PSM.",
								peer_id, set_id, endpoint
							);
							trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Open({:?})", peer_id, connection_id, set_id);
							self.events.push_back(ToSwarm::NotifyHandler {
								peer_id,
								handler: NotifyHandler::One(connection_id),
								event: NotifsHandlerIn::Open {
									protocol_index: set_id.into(),
									peer_id,
								},
							});

							let mut connections = SmallVec::new();
							connections.push((connection_id, ConnectionState::Opening));
							*st = PeerState::Enabled { connections };
						},

						// Poisoned gets inserted above if the entry was missing.
						// Ø | Backoff => Disabled
						st @ &mut PeerState::Poisoned | st @ &mut PeerState::Backoff { .. } => {
							let backoff_until =
								if let PeerState::Backoff { timer_deadline, .. } = st {
									Some(*timer_deadline)
								} else {
									None
								};
							trace!(target: LOG_TARGET,
								"Libp2p => Connected({}, {:?}, {:?}, {:?}): Not requested by PSM, disabling.",
								peer_id, set_id, endpoint, connection_id);

							let mut connections = SmallVec::new();
							connections.push((connection_id, ConnectionState::Closed));
							*st = PeerState::Disabled { connections, backoff_until };
						},

						// In all other states, add this new connection to the list of closed
						// inactive connections.
						PeerState::Incoming { connections, .. } |
						PeerState::Disabled { connections, .. } |
						PeerState::DisabledPendingEnable { connections, .. } |
						PeerState::Enabled { connections, .. } => {
							trace!(target: LOG_TARGET,
								"Libp2p => Connected({}, {:?}, {:?}, {:?}): Secondary connection. Leaving closed.",
								peer_id, set_id, endpoint, connection_id);
							connections.push((connection_id, ConnectionState::Closed));
						},
					}
				}
			},
			FromSwarm::ConnectionClosed(ConnectionClosed { peer_id, connection_id, .. }) => {
				for set_id in (0..self.notif_protocols.len()).map(SetId::from) {
					let mut entry = if let Entry::Occupied(entry) =
						self.peers.entry((peer_id, set_id))
					{
						entry
					} else {
						error!(target: LOG_TARGET, "inject_connection_closed: State mismatch in the custom protos handler");
						debug_assert!(false);
						return
					};

					match mem::replace(entry.get_mut(), PeerState::Poisoned) {
						// Disabled => Disabled | Backoff | Ø
						PeerState::Disabled { mut connections, backoff_until } => {
							trace!(target: LOG_TARGET, "Libp2p => Disconnected({}, {:?}, {:?}): Disabled.",
								peer_id, set_id, connection_id);

							if let Some(pos) =
								connections.iter().position(|(c, _)| *c == connection_id)
							{
								connections.remove(pos);
							} else {
								debug_assert!(false);
								error!(target: LOG_TARGET,
									"inject_connection_closed: State mismatch in the custom protos handler");
							}

							if connections.is_empty() {
								if let Some(until) = backoff_until {
									let now = Instant::now();
									if until > now {
										let delay_id = self.next_delay_id;
										self.next_delay_id.0 += 1;
										let delay = futures_timer::Delay::new(until - now);
										self.delays.push(
											async move {
												delay.await;
												(delay_id, peer_id, set_id)
											}
											.boxed(),
										);

										*entry.get_mut() = PeerState::Backoff {
											timer: delay_id,
											timer_deadline: until,
										};
									} else {
										entry.remove();
									}
								} else {
									entry.remove();
								}
							} else {
								*entry.get_mut() =
									PeerState::Disabled { connections, backoff_until };
							}
						},

						// DisabledPendingEnable => DisabledPendingEnable | Backoff
						PeerState::DisabledPendingEnable {
							mut connections,
							timer_deadline,
							timer,
						} => {
							trace!(
								target: LOG_TARGET,
								"Libp2p => Disconnected({}, {:?}, {:?}): Disabled but pending enable.",
								peer_id, set_id, connection_id
							);

							if let Some(pos) =
								connections.iter().position(|(c, _)| *c == connection_id)
							{
								connections.remove(pos);
							} else {
								error!(target: LOG_TARGET,
									"inject_connection_closed: State mismatch in the custom protos handler");
								debug_assert!(false);
							}

							if connections.is_empty() {
								trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
								self.protocol_controller_handles[usize::from(set_id)]
									.dropped(peer_id);
								*entry.get_mut() = PeerState::Backoff { timer, timer_deadline };
							} else {
								*entry.get_mut() = PeerState::DisabledPendingEnable {
									connections,
									timer_deadline,
									timer,
								};
							}
						},

						// Incoming => Incoming | Disabled | Backoff | Ø
						PeerState::Incoming {
							mut connections,
							backoff_until,
							incoming_index,
							peerset_rejected,
						} => {
							trace!(
								target: LOG_TARGET,
								"Libp2p => Disconnected({}, {:?}, {:?}): OpenDesiredByRemote.",
								peer_id, set_id, connection_id
							);

							debug_assert!(connections
								.iter()
								.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote)));

							if let Some(pos) =
								connections.iter().position(|(c, _)| *c == connection_id)
							{
								connections.remove(pos);
							} else {
								error!(target: LOG_TARGET,
									"inject_connection_closed: State mismatch in the custom protos handler");
								debug_assert!(false);
							}

							let no_desired_left = !connections
								.iter()
								.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote));

							// If no connection is `OpenDesiredByRemote` anymore, clean up the
							// peerset incoming request.
							if no_desired_left {
								// In the incoming state, we don't report "Dropped" straight away.
								// Instead we will report "Dropped" if receive the corresponding
								// "Accept".
								if let Some(state) = self
									.incoming
									.iter_mut()
									.find(|i| i.alive && i.set_id == set_id && i.peer_id == peer_id)
								{
									state.alive = false;
								} else {
									error!(target: LOG_TARGET, "State mismatch in libp2p: no entry in \
										incoming corresponding to an incoming state in peers");
									debug_assert!(false);
								}
							}

							if connections.is_empty() {
								if let Some(until) = backoff_until {
									let now = Instant::now();
									if until > now {
										let delay_id = self.next_delay_id;
										self.next_delay_id.0 += 1;
										let delay = futures_timer::Delay::new(until - now);
										self.delays.push(
											async move {
												delay.await;
												(delay_id, peer_id, set_id)
											}
											.boxed(),
										);

										*entry.get_mut() = PeerState::Backoff {
											timer: delay_id,
											timer_deadline: until,
										};
									} else {
										entry.remove();
									}
								} else {
									entry.remove();
								}
							} else if no_desired_left {
								// If no connection is `OpenDesiredByRemote` anymore, switch to
								// `Disabled`.
								*entry.get_mut() =
									PeerState::Disabled { connections, backoff_until };
							} else {
								*entry.get_mut() = PeerState::Incoming {
									connections,
									backoff_until,
									incoming_index,
									peerset_rejected,
								};
							}
						},

						// Enabled => Enabled | Backoff
						// Peers are always backed-off when disconnecting while Enabled.
						PeerState::Enabled { mut connections } => {
							trace!(
								target: LOG_TARGET,
								"Libp2p => Disconnected({}, {:?}, {:?}): Enabled.",
								peer_id, set_id, connection_id
							);

							debug_assert!(connections.iter().any(|(_, s)| matches!(
								s,
								ConnectionState::Opening | ConnectionState::Open(_)
							)));

							if let Some(pos) =
								connections.iter().position(|(c, _)| *c == connection_id)
							{
								let (_, state) = connections.remove(pos);
								if let ConnectionState::Open(_) = state {
									if let Some((replacement_pos, replacement_sink)) = connections
										.iter()
										.enumerate()
										.find_map(|(num, (_, s))| match s {
											ConnectionState::Open(s) => Some((num, s.clone())),
											_ => None,
										}) {
										if pos <= replacement_pos {
											trace!(
												target: LOG_TARGET,
												"External API <= Sink replaced({}, {:?})",
												peer_id, set_id
											);
											let event = NotificationsOut::CustomProtocolReplaced {
												peer_id,
												set_id,
												notifications_sink: replacement_sink.clone(),
											};
											self.events.push_back(ToSwarm::GenerateEvent(event));
										}
									} else {
										trace!(
											target: LOG_TARGET, "External API <= Closed({}, {:?})",
											peer_id, set_id
										);
										let event = NotificationsOut::CustomProtocolClosed {
											peer_id,
											set_id,
										};
										self.events.push_back(ToSwarm::GenerateEvent(event));
									}
								}
							} else {
								error!(target: LOG_TARGET,
									"inject_connection_closed: State mismatch in the custom protos handler");
								debug_assert!(false);
							}

							if connections.is_empty() {
								trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
								self.protocol_controller_handles[usize::from(set_id)]
									.dropped(peer_id);
								let ban_dur = Uniform::new(5, 10).sample(&mut rand::thread_rng());

								let delay_id = self.next_delay_id;
								self.next_delay_id.0 += 1;
								let delay = futures_timer::Delay::new(Duration::from_secs(ban_dur));
								self.delays.push(
									async move {
										delay.await;
										(delay_id, peer_id, set_id)
									}
									.boxed(),
								);

								*entry.get_mut() = PeerState::Backoff {
									timer: delay_id,
									timer_deadline: Instant::now() + Duration::from_secs(ban_dur),
								};
							} else if !connections.iter().any(|(_, s)| {
								matches!(s, ConnectionState::Opening | ConnectionState::Open(_))
							}) {
								trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
								self.protocol_controller_handles[usize::from(set_id)]
									.dropped(peer_id);

								*entry.get_mut() =
									PeerState::Disabled { connections, backoff_until: None };
							} else {
								*entry.get_mut() = PeerState::Enabled { connections };
							}
						},

						PeerState::Requested |
						PeerState::PendingRequest { .. } |
						PeerState::Backoff { .. } => {
							// This is a serious bug either in this state machine or in libp2p.
							error!(target: LOG_TARGET,
								"`inject_connection_closed` called for unknown peer {}",
								peer_id);
							debug_assert!(false);
						},
						PeerState::Poisoned => {
							error!(target: LOG_TARGET, "State of peer {} is poisoned", peer_id);
							debug_assert!(false);
						},
					}
				}
			},
			FromSwarm::DialFailure(DialFailure { peer_id, error, .. }) => {
				if let DialError::Transport(errors) = error {
					for (addr, error) in errors.iter() {
						trace!(target: LOG_TARGET, "Libp2p => Reach failure for {:?} through {:?}: {:?}", peer_id, addr, error);
					}
				}

				if let Some(peer_id) = peer_id {
					trace!(target: LOG_TARGET, "Libp2p => Dial failure for {:?}", peer_id);

					for set_id in (0..self.notif_protocols.len()).map(SetId::from) {
						if let Entry::Occupied(mut entry) = self.peers.entry((peer_id, set_id)) {
							match mem::replace(entry.get_mut(), PeerState::Poisoned) {
								// The peer is not in our list.
								st @ PeerState::Backoff { .. } => {
									*entry.into_mut() = st;
								},

								// "Basic" situation: we failed to reach a peer that the peerset
								// requested.
								st @ PeerState::Requested |
								st @ PeerState::PendingRequest { .. } => {
									trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
									self.protocol_controller_handles[usize::from(set_id)]
										.dropped(peer_id);

									let now = Instant::now();
									let ban_duration = match st {
										PeerState::PendingRequest { timer_deadline, .. }
											if timer_deadline > now =>
											cmp::max(timer_deadline - now, Duration::from_secs(5)),
										_ => Duration::from_secs(5),
									};

									let delay_id = self.next_delay_id;
									self.next_delay_id.0 += 1;
									let delay = futures_timer::Delay::new(ban_duration);
									self.delays.push(
										async move {
											delay.await;
											(delay_id, peer_id, set_id)
										}
										.boxed(),
									);

									*entry.into_mut() = PeerState::Backoff {
										timer: delay_id,
										timer_deadline: now + ban_duration,
									};
								},

								// We can still get dial failures even if we are already connected
								// to the peer, as an extra diagnostic for an earlier attempt.
								st @ PeerState::Disabled { .. } |
								st @ PeerState::Enabled { .. } |
								st @ PeerState::DisabledPendingEnable { .. } |
								st @ PeerState::Incoming { .. } => {
									*entry.into_mut() = st;
								},

								PeerState::Poisoned => {
									error!(target: LOG_TARGET, "State of {:?} is poisoned", peer_id);
									debug_assert!(false);
								},
							}
						}
					}
				}
			},
			FromSwarm::ListenerClosed(_) => {},
			FromSwarm::ListenFailure(_) => {},
			FromSwarm::ListenerError(_) => {},
			FromSwarm::ExternalAddrExpired(_) => {},
			FromSwarm::NewListener(_) => {},
			FromSwarm::ExpiredListenAddr(_) => {},
			FromSwarm::NewExternalAddrCandidate(_) => {},
			FromSwarm::ExternalAddrConfirmed(_) => {},
			FromSwarm::AddressChange(_) => {},
			FromSwarm::NewListenAddr(_) => {},
			FromSwarm::NewExternalAddrOfPeer(_) => {},
			event => {
				warn!(target: LOG_TARGET, "New unknown `FromSwarm` libp2p event: {event:?}");
			},
		}
	}

	fn on_connection_handler_event(
		&mut self,
		peer_id: PeerId,
		connection_id: ConnectionId,
		event: THandlerOutEvent<Self>,
	) {
		match event {
			NotifsHandlerOut::OpenDesiredByRemote { protocol_index, handshake } => {
				let set_id = SetId::from(protocol_index);

				trace!(target: LOG_TARGET,
					"Handler({:?}, {:?}]) => OpenDesiredByRemote({:?})",
					peer_id, connection_id, set_id);

				let mut entry = if let Entry::Occupied(entry) = self.peers.entry((peer_id, set_id))
				{
					entry
				} else {
					error!(
						target: LOG_TARGET,
						"OpenDesiredByRemote: State mismatch in the custom protos handler"
					);
					debug_assert!(false);
					return
				};

				match mem::replace(entry.get_mut(), PeerState::Poisoned) {
					// Incoming => Incoming
					PeerState::Incoming {
						mut connections,
						backoff_until,
						incoming_index,
						peerset_rejected,
					} => {
						debug_assert!(connections
							.iter()
							.any(|(_, s)| matches!(s, ConnectionState::OpenDesiredByRemote)));
						if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, _)| *c == connection_id)
						{
							if let ConnectionState::Closed = *connec_state {
								*connec_state = ConnectionState::OpenDesiredByRemote;
							} else {
								// Connections in `OpeningThenClosing` and `Closing` state can be
								// in a Closed phase, and as such can emit `OpenDesiredByRemote`
								// messages.
								// Since an `Open` and/or a `Close` message have already been sent,
								// there is nothing much that can be done about this anyway.
								debug_assert!(matches!(
									connec_state,
									ConnectionState::OpeningThenClosing | ConnectionState::Closing
								));
							}
						} else {
							error!(
								target: LOG_TARGET,
								"OpenDesiredByRemote: State mismatch in the custom protos handler"
							);
							debug_assert!(false);
						}

						*entry.into_mut() = PeerState::Incoming {
							connections,
							backoff_until,
							incoming_index,
							peerset_rejected,
						};
					},

					PeerState::Enabled { mut connections } => {
						debug_assert!(connections.iter().any(|(_, s)| matches!(
							s,
							ConnectionState::Opening | ConnectionState::Open(_)
						)));

						if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, _)| *c == connection_id)
						{
							if let ConnectionState::Closed = *connec_state {
								trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Open({:?})",
									peer_id, connection_id, set_id);
								self.events.push_back(ToSwarm::NotifyHandler {
									peer_id,
									handler: NotifyHandler::One(connection_id),
									event: NotifsHandlerIn::Open {
										protocol_index: set_id.into(),
										peer_id,
									},
								});
								*connec_state = ConnectionState::Opening;
							} else {
								// Connections in `OpeningThenClosing`, `Opening`, and `Closing`
								// state can be in a Closed phase, and as such can emit
								// `OpenDesiredByRemote` messages.
								// Since an `Open` message haS already been sent, there is nothing
								// more to do.
								debug_assert!(matches!(
									connec_state,
									ConnectionState::OpenDesiredByRemote |
										ConnectionState::Closing | ConnectionState::Opening
								));
							}
						} else {
							error!(
								target: LOG_TARGET,
								"OpenDesiredByRemote: State mismatch in the custom protos handler"
							);
							debug_assert!(false);
						}

						*entry.into_mut() = PeerState::Enabled { connections };
					},

					// Disabled => Disabled | Incoming
					PeerState::Disabled { mut connections, backoff_until } => {
						if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, _)| *c == connection_id)
						{
							if let ConnectionState::Closed = *connec_state {
								*connec_state = ConnectionState::OpenDesiredByRemote;

								let incoming_id = self.next_incoming_index;
								self.next_incoming_index.0 += 1;

								trace!(target: LOG_TARGET, "PSM <= Incoming({}, {:?}, {:?}).",
									peer_id, set_id, incoming_id);
								self.protocol_controller_handles[usize::from(set_id)]
									.incoming_connection(peer_id, incoming_id);
								self.incoming.push(IncomingPeer {
									peer_id,
									set_id,
									alive: true,
									incoming_id,
									handshake,
								});

								*entry.into_mut() = PeerState::Incoming {
									connections,
									backoff_until,
									peerset_rejected: false,
									incoming_index: incoming_id,
								};
							} else {
								// Connections in `OpeningThenClosing` and `Closing` state can be
								// in a Closed phase, and as such can emit `OpenDesiredByRemote`
								// messages.
								// We ignore them.
								debug_assert!(matches!(
									connec_state,
									ConnectionState::OpeningThenClosing | ConnectionState::Closing
								));
								*entry.into_mut() =
									PeerState::Disabled { connections, backoff_until };
							}
						} else {
							error!(
								target: LOG_TARGET,
								"OpenDesiredByRemote: State mismatch in the custom protos handler"
							);
							debug_assert!(false);
						}
					},

					// DisabledPendingEnable => Enabled | DisabledPendingEnable
					PeerState::DisabledPendingEnable { mut connections, timer, timer_deadline } => {
						if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, _)| *c == connection_id)
						{
							if let ConnectionState::Closed = *connec_state {
								trace!(target: LOG_TARGET, "Handler({:?}, {:?}) <= Open({:?})",
									peer_id, connection_id, set_id);
								self.events.push_back(ToSwarm::NotifyHandler {
									peer_id,
									handler: NotifyHandler::One(connection_id),
									event: NotifsHandlerIn::Open {
										protocol_index: set_id.into(),
										peer_id,
									},
								});
								*connec_state = ConnectionState::Opening;

								*entry.into_mut() = PeerState::Enabled { connections };
							} else {
								// Connections in `OpeningThenClosing` and `Closing` state can be
								// in a Closed phase, and as such can emit `OpenDesiredByRemote`
								// messages.
								// We ignore them.
								debug_assert!(matches!(
									connec_state,
									ConnectionState::OpeningThenClosing | ConnectionState::Closing
								));
								*entry.into_mut() = PeerState::DisabledPendingEnable {
									connections,
									timer,
									timer_deadline,
								};
							}
						} else {
							error!(
								target: LOG_TARGET,
								"OpenDesiredByRemote: State mismatch in the custom protos handler"
							);
							debug_assert!(false);
						}
					},

					state => {
						error!(target: LOG_TARGET,
							   "OpenDesiredByRemote: Unexpected state in the custom protos handler: {:?}",
							   state);
						debug_assert!(false);
					},
				};
			},

			NotifsHandlerOut::CloseDesired { protocol_index, reason } => {
				let set_id = SetId::from(protocol_index);

				trace!(target: LOG_TARGET,
					"Handler({}, {:?}) => CloseDesired({:?})",
					peer_id, connection_id, set_id);

				let mut entry = if let Entry::Occupied(entry) = self.peers.entry((peer_id, set_id))
				{
					entry
				} else {
					error!(target: LOG_TARGET, "CloseDesired: State mismatch in the custom protos handler");
					debug_assert!(false);
					return
				};

				if reason == CloseReason::ProtocolMisbehavior {
					self.events.push_back(ToSwarm::GenerateEvent(
						NotificationsOut::ProtocolMisbehavior { peer_id, set_id },
					));
				}

				match mem::replace(entry.get_mut(), PeerState::Poisoned) {
					// Enabled => Enabled | Disabled
					PeerState::Enabled { mut connections } => {
						debug_assert!(connections.iter().any(|(_, s)| matches!(
							s,
							ConnectionState::Opening | ConnectionState::Open(_)
						)));

						let pos = if let Some(pos) =
							connections.iter().position(|(c, _)| *c == connection_id)
						{
							pos
						} else {
							error!(target: LOG_TARGET,
								"CloseDesired: State mismatch in the custom protos handler");
							debug_assert!(false);
							return
						};

						if matches!(connections[pos].1, ConnectionState::Closing) {
							*entry.into_mut() = PeerState::Enabled { connections };
							return
						}

						debug_assert!(matches!(connections[pos].1, ConnectionState::Open(_)));
						connections[pos].1 = ConnectionState::Closing;

						trace!(target: LOG_TARGET, "Handler({}, {:?}) <= Close({:?})", peer_id, connection_id, set_id);
						self.events.push_back(ToSwarm::NotifyHandler {
							peer_id,
							handler: NotifyHandler::One(connection_id),
							event: NotifsHandlerIn::Close { protocol_index: set_id.into() },
						});

						if let Some((replacement_pos, replacement_sink)) =
							connections.iter().enumerate().find_map(|(num, (_, s))| match s {
								ConnectionState::Open(s) => Some((num, s.clone())),
								_ => None,
							}) {
							if pos <= replacement_pos {
								trace!(target: LOG_TARGET, "External API <= Sink replaced({:?}, {:?})", peer_id, set_id);
								let event = NotificationsOut::CustomProtocolReplaced {
									peer_id,
									set_id,
									notifications_sink: replacement_sink.clone(),
								};
								self.events.push_back(ToSwarm::GenerateEvent(event));
							}

							*entry.into_mut() = PeerState::Enabled { connections };
						} else {
							// List of open connections wasn't empty before but now it is.
							if !connections
								.iter()
								.any(|(_, s)| matches!(s, ConnectionState::Opening))
							{
								trace!(target: LOG_TARGET, "PSM <= Dropped({}, {:?})", peer_id, set_id);
								self.protocol_controller_handles[usize::from(set_id)]
									.dropped(peer_id);
								*entry.into_mut() =
									PeerState::Disabled { connections, backoff_until: None };
							} else {
								*entry.into_mut() = PeerState::Enabled { connections };
							}

							trace!(target: LOG_TARGET, "External API <= Closed({}, {:?})", peer_id, set_id);
							let event = NotificationsOut::CustomProtocolClosed { peer_id, set_id };
							self.events.push_back(ToSwarm::GenerateEvent(event));
						}
					},

					// All connections in `Disabled` and `DisabledPendingEnable` have been sent a
					// `Close` message already, and as such ignore any `CloseDesired` message.
					state @ PeerState::Disabled { .. } |
					state @ PeerState::DisabledPendingEnable { .. } => {
						*entry.into_mut() = state;
					},
					state => {
						error!(target: LOG_TARGET,
							"Unexpected state in the custom protos handler: {:?}",
							state);
					},
				}
			},

			NotifsHandlerOut::CloseResult { protocol_index } => {
				let set_id = SetId::from(protocol_index);

				trace!(target: LOG_TARGET,
					"Handler({}, {:?}) => CloseResult({:?})",
					peer_id, connection_id, set_id);

				match self.peers.get_mut(&(peer_id, set_id)) {
					// Move the connection from `Closing` to `Closed`.
					Some(PeerState::Incoming { connections, .. }) |
					Some(PeerState::DisabledPendingEnable { connections, .. }) |
					Some(PeerState::Disabled { connections, .. }) |
					Some(PeerState::Enabled { connections, .. }) => {
						if let Some((_, connec_state)) = connections.iter_mut().find(|(c, s)| {
							*c == connection_id && matches!(s, ConnectionState::Closing)
						}) {
							*connec_state = ConnectionState::Closed;
						} else {
							error!(target: LOG_TARGET,
								"CloseResult: State mismatch in the custom protos handler");
							debug_assert!(false);
						}
					},

					state => {
						error!(target: LOG_TARGET,
							   "CloseResult: Unexpected state in the custom protos handler: {:?}",
							   state);
						debug_assert!(false);
					},
				}
			},

			NotifsHandlerOut::OpenResultOk {
				protocol_index,
				negotiated_fallback,
				received_handshake,
				notifications_sink,
				inbound,
				..
			} => {
				let set_id = SetId::from(protocol_index);
				trace!(target: LOG_TARGET,
					"Handler({}, {:?}) => OpenResultOk({:?})",
					peer_id, connection_id, set_id);

				match self.peers.get_mut(&(peer_id, set_id)) {
					Some(PeerState::Enabled { connections, .. }) => {
						debug_assert!(connections.iter().any(|(_, s)| matches!(
							s,
							ConnectionState::Opening | ConnectionState::Open(_)
						)));
						let any_open =
							connections.iter().any(|(_, s)| matches!(s, ConnectionState::Open(_)));

						if let Some((_, connec_state)) = connections.iter_mut().find(|(c, s)| {
							*c == connection_id && matches!(s, ConnectionState::Opening)
						}) {
							if !any_open {
								trace!(target: LOG_TARGET, "External API <= Open({}, {:?})", peer_id, set_id);
								let event = NotificationsOut::CustomProtocolOpen {
									peer_id,
									set_id,
									direction: if inbound {
										Direction::Inbound
									} else {
										Direction::Outbound
									},
									received_handshake: received_handshake.clone(),
									negotiated_fallback: negotiated_fallback.clone(),
									notifications_sink: notifications_sink.clone(),
								};
								self.events.push_back(ToSwarm::GenerateEvent(event));
							}
							*connec_state = ConnectionState::Open(notifications_sink);
						} else if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, s)| {
								*c == connection_id &&
									matches!(s, ConnectionState::OpeningThenClosing)
							}) {
							*connec_state = ConnectionState::Closing;
						} else {
							error!(target: LOG_TARGET,
								"OpenResultOk State mismatch in the custom protos handler");
							debug_assert!(false);
						}
					},

					Some(PeerState::Incoming { connections, .. }) |
					Some(PeerState::DisabledPendingEnable { connections, .. }) |
					Some(PeerState::Disabled { connections, .. }) => {
						if let Some((_, connec_state)) = connections.iter_mut().find(|(c, s)| {
							*c == connection_id && matches!(s, ConnectionState::OpeningThenClosing)
						}) {
							*connec_state = ConnectionState::Closing;
						} else {
							error!(target: LOG_TARGET,
								"OpenResultOk State mismatch in the custom protos handler");
							debug_assert!(false);
						}
					},

					state => {
						error!(target: LOG_TARGET,
							   "OpenResultOk: Unexpected state in the custom protos handler: {:?}",
							   state);
						debug_assert!(false);
					},
				}
			},

			NotifsHandlerOut::OpenResultErr { protocol_index } => {
				let set_id = SetId::from(protocol_index);
				trace!(target: LOG_TARGET,
					"Handler({:?}, {:?}) => OpenResultErr({:?})",
					peer_id, connection_id, set_id);

				let mut entry = if let Entry::Occupied(entry) = self.peers.entry((peer_id, set_id))
				{
					entry
				} else {
					error!(target: LOG_TARGET, "OpenResultErr: State mismatch in the custom protos handler");
					debug_assert!(false);
					return
				};

				match mem::replace(entry.get_mut(), PeerState::Poisoned) {
					PeerState::Enabled { mut connections } => {
						debug_assert!(connections.iter().any(|(_, s)| matches!(
							s,
							ConnectionState::Opening | ConnectionState::Open(_)
						)));

						if let Some((_, connec_state)) = connections.iter_mut().find(|(c, s)| {
							*c == connection_id && matches!(s, ConnectionState::Opening)
						}) {
							*connec_state = ConnectionState::Closed;
						} else if let Some((_, connec_state)) =
							connections.iter_mut().find(|(c, s)| {
								*c == connection_id &&
									matches!(s, ConnectionState::OpeningThenClosing)
							}) {
							*connec_state = ConnectionState::Closing;
						} else {
							error!(target: LOG_TARGET,
								"OpenResultErr: State mismatch in the custom protos handler");
							debug_assert!(false);
						}

						if !connections.iter().any(|(_, s)| {
							matches!(s, ConnectionState::Opening | ConnectionState::Open(_))
						}) {
							trace!(target: LOG_TARGET, "PSM <= Dropped({:?}, {:?})", peer_id, set_id);
							self.protocol_controller_handles[usize::from(set_id)].dropped(peer_id);

							let ban_dur = Uniform::new(5, 10).sample(&mut rand::thread_rng());
							*entry.into_mut() = PeerState::Disabled {
								connections,
								backoff_until: Some(Instant::now() + Duration::from_secs(ban_dur)),
							};
						} else {
							*entry.into_mut() = PeerState::Enabled { connections };
						}
					},
					mut state @ PeerState::Incoming { .. } |
					mut state @ PeerState::DisabledPendingEnable { .. } |
					mut state @ PeerState::Disabled { .. } => {
						match &mut state {
							PeerState::Incoming { connections, .. } |
							PeerState::Disabled { connections, .. } |
							PeerState::DisabledPendingEnable { connections, .. } => {
								if let Some((_, connec_state)) =
									connections.iter_mut().find(|(c, s)| {
										*c == connection_id &&
											matches!(s, ConnectionState::OpeningThenClosing)
									}) {
									*connec_state = ConnectionState::Closing;
								} else {
									error!(target: LOG_TARGET,
										"OpenResultErr: State mismatch in the custom protos handler");
									debug_assert!(false);
								}
							},
							_ => unreachable!(
								"Match branches are the same as the one on which we
							enter this block; qed"
							),
						};

						*entry.into_mut() = state;
					},
					state => {
						error!(target: LOG_TARGET,
							"Unexpected state in the custom protos handler: {:?}",
							state);
						debug_assert!(false);
					},
				};
			},

			NotifsHandlerOut::Notification { protocol_index, message } => {
				let set_id = SetId::from(protocol_index);
				if self.is_open(&peer_id, set_id) {
					trace!(
						target: LOG_TARGET,
						"Handler({:?}) => Notification({}, {:?}, {} bytes)",
						connection_id,
						peer_id,
						set_id,
						message.len()
					);
					trace!(
						target: LOG_TARGET,
						"External API <= Message({}, {:?})",
						peer_id,
						set_id,
					);
					let event = NotificationsOut::Notification {
						peer_id,
						set_id,
						message: message.clone(),
					};
					self.events.push_back(ToSwarm::GenerateEvent(event));
				} else {
					trace!(
						target: LOG_TARGET,
						"Handler({:?}) => Post-close notification({}, {:?}, {} bytes)",
						connection_id,
						peer_id,
						set_id,
						message.len()
					);
				}
			},
			NotifsHandlerOut::Close { protocol_index } => {
				let set_id = SetId::from(protocol_index);

				trace!(target: LOG_TARGET, "Handler({}, {:?}) => SyncNotificationsClogged({:?})", peer_id, connection_id, set_id);
				self.events.push_back(ToSwarm::CloseConnection {
					peer_id,
					connection: CloseConnection::One(connection_id),
				});
			},
		}
	}

	fn poll(&mut self, cx: &mut Context) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
		if let Some(event) = self.events.pop_front() {
			return Poll::Ready(event)
		}

		// Poll for instructions from the protocol controllers.
		loop {
			match futures::Stream::poll_next(Pin::new(&mut self.from_protocol_controllers), cx) {
				Poll::Ready(Some(Message::Accept(index))) => {
					self.peerset_report_preaccept(index);
				},
				Poll::Ready(Some(Message::Reject(index))) => {
					let _ = self.peerset_report_reject(index);
				},
				Poll::Ready(Some(Message::Connect { peer_id, set_id, .. })) => {
					self.peerset_report_connect(peer_id, set_id);
				},
				Poll::Ready(Some(Message::Drop { peer_id, set_id, .. })) => {
					self.peerset_report_disconnect(peer_id, set_id);
				},
				Poll::Ready(None) => {
					error!(
						target: LOG_TARGET,
						"Protocol controllers receiver stream has returned `None`. Ignore this error if the node is shutting down.",
					);
					break
				},
				Poll::Pending => break,
			}
		}

		// poll commands from protocols
		loop {
			match futures::Stream::poll_next(Pin::new(&mut self.command_streams), cx) {
				Poll::Ready(Some((set_id, command))) => match command {
					NotificationCommand::SetHandshake(handshake) => {
						self.set_notif_protocol_handshake(set_id.into(), handshake);
					},
					NotificationCommand::OpenSubstream(_peer) |
					NotificationCommand::CloseSubstream(_peer) => {
						todo!("substream control not implemented");
					},
				},
				Poll::Ready(None) => {
					error!(target: LOG_TARGET, "Protocol command streams have been shut down");
					break
				},
				Poll::Pending => break,
			}
		}

		while let Poll::Ready(Some((result, index))) =
			self.pending_inbound_validations.poll_next_unpin(cx)
		{
			match result {
				Ok(ValidationResult::Accept) => {
					self.protocol_report_accept(index);
				},
				Ok(ValidationResult::Reject) => {
					self.protocol_report_reject(index);
				},
				Err(_) => {
					error!(target: LOG_TARGET, "Protocol has shut down");
					break
				},
			}
		}

		while let Poll::Ready(Some((delay_id, peer_id, set_id))) =
			Pin::new(&mut self.delays).poll_next(cx)
		{
			let peer_state = match self.peers.get_mut(&(peer_id, set_id)) {
				Some(s) => s,
				// We intentionally never remove elements from `delays`, and it may
				// thus contain peers which are now gone. This is a normal situation.
				None => continue,
			};

			match peer_state {
				PeerState::Backoff { timer, .. } if *timer == delay_id => {
					trace!(target: LOG_TARGET, "Libp2p <= Clean up ban of {:?} from the state ({:?})", peer_id, set_id);
					self.peers.remove(&(peer_id, set_id));
				},

				PeerState::PendingRequest { timer, .. } if *timer == delay_id => {
					trace!(target: LOG_TARGET, "Libp2p <= Dial {:?} now that ban has expired ({:?})", peer_id, set_id);
					self.events.push_back(ToSwarm::Dial { opts: peer_id.into() });
					*peer_state = PeerState::Requested;
				},

				PeerState::DisabledPendingEnable { connections, timer, timer_deadline }
					if *timer == delay_id =>
				{
					// The first element of `closed` is chosen to open the notifications substream.
					if let Some((connec_id, connec_state)) =
						connections.iter_mut().find(|(_, s)| matches!(s, ConnectionState::Closed))
					{
						trace!(target: LOG_TARGET, "Handler({}, {:?}) <= Open({:?}) (ban expired)",
							peer_id, *connec_id, set_id);
						self.events.push_back(ToSwarm::NotifyHandler {
							peer_id,
							handler: NotifyHandler::One(*connec_id),
							event: NotifsHandlerIn::Open { protocol_index: set_id.into(), peer_id },
						});
						*connec_state = ConnectionState::Opening;
						*peer_state = PeerState::Enabled { connections: mem::take(connections) };
					} else {
						*timer_deadline = Instant::now() + Duration::from_secs(5);
						let delay = futures_timer::Delay::new(Duration::from_secs(5));
						let timer = *timer;
						self.delays.push(
							async move {
								delay.await;
								(timer, peer_id, set_id)
							}
							.boxed(),
						);
					}
				},

				// We intentionally never remove elements from `delays`, and it may
				// thus contain obsolete entries. This is a normal situation.
				_ => {},
			}
		}

		if let Some(event) = self.events.pop_front() {
			return Poll::Ready(event)
		}

		Poll::Pending
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		mock::MockPeerStore,
		protocol::notifications::handler::tests::*,
		protocol_controller::{IncomingIndex, ProtoSetConfig, ProtocolController},
	};
	use libp2p::core::ConnectedPoint;
	use sc_utils::mpsc::tracing_unbounded;
	use std::{collections::HashSet, iter};

	impl PartialEq for ConnectionState {
		fn eq(&self, other: &ConnectionState) -> bool {
			match (self, other) {
				(ConnectionState::Closed, ConnectionState::Closed) => true,
				(ConnectionState::Closing, ConnectionState::Closing) => true,
				(ConnectionState::Opening, ConnectionState::Opening) => true,
				(ConnectionState::OpeningThenClosing, ConnectionState::OpeningThenClosing) => true,
				(ConnectionState::OpenDesiredByRemote, ConnectionState::OpenDesiredByRemote) =>
					true,
				(ConnectionState::Open(_), ConnectionState::Open(_)) => true,
				_ => false,
			}
		}
	}

	fn development_notifs(
	) -> (Notifications, ProtocolController, Box<dyn crate::service::traits::NotificationService>)
	{
		let (protocol_handle_pair, notif_service) =
			crate::protocol::notifications::service::notification_service("/proto/1".into());
		let (to_notifications, from_controller) =
			tracing_unbounded("test_controller_to_notifications", 10_000);

		let (handle, controller) = ProtocolController::new(
			SetId::from(0),
			ProtoSetConfig {
				in_peers: 25,
				out_peers: 25,
				reserved_nodes: HashSet::new(),
				reserved_only: false,
			},
			to_notifications,
			Arc::new(MockPeerStore {}),
		);

		let (notif_handle, command_stream) = protocol_handle_pair.split();
		(
			Notifications::new(
				vec![handle],
				from_controller,
				NotificationMetrics::new(None),
				iter::once((
					ProtocolConfig {
						name: "/foo".into(),
						fallback_names: Vec::new(),
						handshake: vec![1, 2, 3, 4],
						max_notification_size: u64::MAX,
					},
					notif_handle,
					command_stream,
				)),
			),
			controller,
			notif_service,
		)
	}

	#[test]
	fn update_handshake() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		let inner = notif.notif_protocols.get_mut(0).unwrap().handshake.read().clone();
		assert_eq!(inner, vec![1, 2, 3, 4]);

		notif.set_notif_protocol_handshake(0.into(), vec![5, 6, 7, 8]);

		let inner = notif.notif_protocols.get_mut(0).unwrap().handshake.read().clone();
		assert_eq!(inner, vec![5, 6, 7, 8]);
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn update_unknown_handshake() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		notif.set_notif_protocol_handshake(1337.into(), vec![5, 6, 7, 8]);
	}

	#[test]
	fn disconnect_backoff_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		let peer = PeerId::random();
		notif.peers.insert(
			(peer, 0.into()),
			PeerState::Backoff { timer: DelayId(0), timer_deadline: Instant::now() },
		);
		notif.disconnect_peer(&peer, 0.into());

		assert!(std::matches!(
			notif.peers.get(&(peer, 0.into())),
			Some(PeerState::Backoff { timer: DelayId(0), .. })
		));
	}

	#[test]
	fn disconnect_pending_request() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();

		notif.peers.insert(
			(peer, 0.into()),
			PeerState::PendingRequest { timer: DelayId(0), timer_deadline: Instant::now() },
		);
		notif.disconnect_peer(&peer, 0.into());

		assert!(std::matches!(
			notif.peers.get(&(peer, 0.into())),
			Some(PeerState::PendingRequest { timer: DelayId(0), .. })
		));
	}

	#[test]
	fn disconnect_requested_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		let peer = PeerId::random();
		notif.peers.insert((peer, 0.into()), PeerState::Requested);
		notif.disconnect_peer(&peer, 0.into());

		assert!(std::matches!(notif.peers.get(&(peer, 0.into())), Some(PeerState::Requested)));
	}

	#[test]
	fn disconnect_disabled_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		notif.peers.insert(
			(peer, 0.into()),
			PeerState::Disabled { backoff_until: None, connections: SmallVec::new() },
		);
		notif.disconnect_peer(&peer, 0.into());

		assert!(std::matches!(
			notif.peers.get(&(peer, 0.into())),
			Some(PeerState::Disabled { backoff_until: None, .. })
		));
	}

	#[test]
	fn remote_opens_connection_and_substream() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));

		if let Some(&PeerState::Disabled { ref connections, backoff_until: None }) =
			notif.peers.get(&(peer, 0.into()))
		{
			assert_eq!(connections[0], (conn, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);

		if let Some(&PeerState::Incoming { ref connections, backoff_until: None, .. }) =
			notif.peers.get(&(peer, 0.into()))
		{
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0], (conn, ConnectionState::OpenDesiredByRemote));
		} else {
			panic!("invalid state");
		}

		assert!(std::matches!(
			notif.incoming.pop(),
			Some(IncomingPeer { alive: true, incoming_id: IncomingIndex(0), .. }),
		));
	}

	#[tokio::test]
	async fn disconnect_remote_substream_before_handled_by_controller() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		notif.disconnect_peer(&peer, 0.into());

		if let Some(&PeerState::Disabled { ref connections, backoff_until: None }) =
			notif.peers.get(&(peer, 0.into()))
		{
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0], (conn, ConnectionState::Closing));
		} else {
			panic!("invalid state");
		}
	}

	#[test]
	fn peerset_report_connect_backoff() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		//
		// there is not straight-forward way of adding backoff to `PeerState::Disabled`
		// so manually adjust the value in order to progress on to the next stage.
		// This modification together with `ConnectionClosed` will convert the peer
		// state into `PeerState::Backoff`.
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		let timer = if let Some(&PeerState::Backoff { timer_deadline, .. }) =
			notif.peers.get(&(peer, set_id))
		{
			timer_deadline
		} else {
			panic!("invalid state");
		};

		// attempt to connect the backed-off peer and verify that the request is pending
		notif.peerset_report_connect(peer, set_id);

		if let Some(&PeerState::PendingRequest { timer_deadline, .. }) =
			notif.peers.get(&(peer, set_id))
		{
			assert_eq!(timer, timer_deadline);
		} else {
			panic!("invalid state");
		}
	}

	#[test]
	fn peerset_connect_incoming() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);

		// attempt to connect to the peer and verify that the peer state is `Enabled`;
		// we rely on implementation detail that incoming indices are counted from 0
		// to not mock the `Peerset`
		notif.protocol_report_accept(IncomingIndex(0));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));
	}

	#[test]
	fn peerset_disconnect_disable_pending_enable() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		// switch state to `DisabledPendingEnable`
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));

		notif.peerset_report_disconnect(peer, set_id);

		if let Some(PeerState::Disabled { backoff_until, .. }) = notif.peers.get(&(peer, set_id)) {
			assert!(backoff_until.is_some());
			assert!(backoff_until.unwrap() > Instant::now());
		} else {
			panic!("invalid state");
		}
	}

	#[test]
	fn peerset_disconnect_enabled() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		// Set peer into `Enabled` state.
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		// we rely on the implementation detail that incoming indices are counted from 0
		// to not mock the `Peerset`
		notif.protocol_report_accept(IncomingIndex(0));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		// disconnect peer and verify that the state is `Disabled`
		notif.peerset_report_disconnect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));
	}

	#[test]
	fn peerset_disconnect_requested() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);

		// Set peer into `Requested` state.
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Requested)));

		// disconnect peer and verify that the state is `Disabled`
		notif.peerset_report_disconnect(peer, set_id);
		assert!(notif.peers.get(&(peer, set_id)).is_none());
	}

	#[test]
	fn peerset_disconnect_pending_request() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));

		// attempt to connect the backed-off peer and verify that the request is pending
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::PendingRequest { .. })
		));

		// attempt to disconnect the backed-off peer and verify that the request is pending
		notif.peerset_report_disconnect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));
	}

	#[test]
	fn peerset_accept_peer_not_alive() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		assert!(std::matches!(
			notif.incoming[0],
			IncomingPeer { alive: true, incoming_id: IncomingIndex(0), .. },
		));

		notif.disconnect_peer(&peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));
		assert!(std::matches!(
			notif.incoming[0],
			IncomingPeer { alive: false, incoming_id: IncomingIndex(0), .. },
		));

		notif.protocol_report_accept(IncomingIndex(0));
		assert_eq!(notif.incoming.len(), 0);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(PeerState::Disabled { .. })));
	}

	#[test]
	fn secondary_connection_peer_state_incoming() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		if let Some(PeerState::Incoming { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0], (conn, ConnectionState::OpenDesiredByRemote));
		} else {
			panic!("invalid state");
		}

		// add another connection
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn2,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));

		if let Some(PeerState::Incoming { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections.len(), 2);
			assert_eq!(connections[0], (conn, ConnectionState::OpenDesiredByRemote));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}
	}

	#[test]
	fn close_connection_for_disabled_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(notif.peers.get(&(peer, set_id)).is_none());
	}

	#[test]
	fn close_connection_for_incoming_peer_one_connection() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(notif.peers.get(&(peer, set_id)).is_none());
		assert!(std::matches!(
			notif.incoming[0],
			IncomingPeer { alive: false, incoming_id: IncomingIndex(0), .. },
		));
	}

	#[test]
	fn close_connection_for_incoming_peer_two_connections() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let conn1 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conns = SmallVec::<
			[(ConnectionId, ConnectionState); crate::MAX_CONNECTIONS_PER_PEER],
		>::from(vec![(conn, ConnectionState::Closed)]);

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		conns.push((conn1, ConnectionState::Closed));

		if let Some(PeerState::Incoming { ref connections, .. }) = notif.peers.get(&(peer, set_id))
		{
			assert_eq!(connections.len(), 2);
			assert_eq!(connections[0], (conn, ConnectionState::OpenDesiredByRemote));
			assert_eq!(connections[1], (conn1, ConnectionState::Closed));
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		if let Some(PeerState::Disabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0], (conn1, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}
	}

	#[test]
	fn connection_and_substream_open() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		// move the peer to `Enabled` state
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		// We rely on the implementation detail that incoming indices are counted
		// from 0 to not mock the `Peerset`.
		notif.protocol_report_accept(IncomingIndex(0));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		// open new substream
		let event = conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]);

		notif.on_connection_handler_event(peer, conn, event);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0].0, conn);
			assert!(std::matches!(connections[0].1, ConnectionState::Open(_)));
		}

		assert!(std::matches!(
			notif.events[notif.events.len() - 1],
			ToSwarm::GenerateEvent(NotificationsOut::CustomProtocolOpen { .. })
		));
	}

	#[test]
	fn connection_closed_sink_replaced() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		// open two connections
		for conn_id in vec![conn1, conn2] {
			notif.on_swarm_event(FromSwarm::ConnectionEstablished(
				libp2p::swarm::behaviour::ConnectionEstablished {
					peer_id: peer,
					connection_id: conn_id,
					endpoint: &connected,
					failed_addresses: &[],
					other_established: 0usize,
				},
			));
		}

		if let Some(PeerState::Disabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Closed));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		// open substreams on both active connections
		notif.peerset_report_connect(peer, set_id);
		notif.on_connection_handler_event(
			peer,
			conn2,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);

		if let Some(PeerState::Enabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Opening));
			assert_eq!(connections[1], (conn2, ConnectionState::Opening));
		} else {
			panic!("invalid state");
		}

		// add two new substreams, one for each connection and verify that both are in open state
		for conn in vec![conn1, conn2].iter() {
			notif.on_connection_handler_event(
				peer,
				*conn,
				conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]),
			);
		}

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0].0, conn1);
			assert!(std::matches!(connections[0].1, ConnectionState::Open(_)));
			assert_eq!(connections[1].0, conn2);
			assert!(std::matches!(connections[1].1, ConnectionState::Open(_)));
		} else {
			panic!("invalid state");
		}

		// check peer information
		assert_eq!(notif.open_peers().collect::<Vec<_>>(), vec![&peer],);

		// close the other connection and verify that notification replacement event is emitted
		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections.len(), 1);
			assert_eq!(connections[0].0, conn2);
			assert!(std::matches!(connections[0].1, ConnectionState::Open(_)));
		} else {
			panic!("invalid state");
		}

		assert!(std::matches!(
			notif.events[notif.events.len() - 1],
			ToSwarm::GenerateEvent(NotificationsOut::CustomProtocolReplaced { .. })
		));
	}

	#[test]
	fn dial_failure_for_requested_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);

		// Set peer into `Requested` state.
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Requested)));

		notif.on_swarm_event(FromSwarm::DialFailure(libp2p::swarm::behaviour::DialFailure {
			peer_id: Some(peer),
			error: &libp2p::swarm::DialError::Aborted,
			connection_id: ConnectionId::new_unchecked(1337),
		}));

		if let Some(PeerState::Backoff { timer_deadline, .. }) = notif.peers.get(&(peer, set_id)) {
			assert!(timer_deadline > &Instant::now());
		} else {
			panic!("invalid state");
		}
	}

	#[tokio::test]
	async fn write_notification() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]),
		);

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0].0, conn);
			assert!(std::matches!(connections[0].1, ConnectionState::Open(_)));
		} else {
			panic!("invalid state");
		}

		notif
			.peers
			.get(&(peer, set_id))
			.unwrap()
			.get_open()
			.unwrap()
			.send_sync_notification(vec![1, 3, 3, 7]);
		assert_eq!(conn_yielder.get_next_event(peer, set_id.into()).await, Some(vec![1, 3, 3, 7]));
	}

	#[test]
	fn peerset_report_connect_backoff_expired() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let backoff_duration = Duration::from_millis(100);

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until = Some(Instant::now().checked_add(backoff_duration).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		// wait until the backoff time has passed
		std::thread::sleep(backoff_duration * 2);

		// attempt to connect the backed-off peer and verify that the request is pending
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Requested { .. })))
	}

	#[test]
	fn peerset_report_disconnect_disabled() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.peerset_report_disconnect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));
	}

	#[test]
	fn peerset_report_disconnect_backoff() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let backoff_duration = Duration::from_secs(2);

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until = Some(Instant::now().checked_add(backoff_duration).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));

		notif.peerset_report_disconnect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));
	}

	#[test]
	fn peer_is_backed_off_if_both_connections_get_closed_while_peer_is_disabled_with_back_off() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn2,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		// switch state to `DisabledPendingEnable`
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn2,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));
	}

	#[test]
	fn inject_connection_closed_incoming_with_backoff() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);

		// manually add backoff for the entry
		if let Some(&mut PeerState::Incoming { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, 0.into()))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		} else {
			panic!("invalid state");
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));
	}

	#[test]
	fn two_connections_inactive_connection_gets_closed_peer_state_is_still_incoming() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		// open two connections
		for conn_id in vec![conn1, conn2] {
			notif.on_swarm_event(FromSwarm::ConnectionEstablished(
				libp2p::swarm::behaviour::ConnectionEstablished {
					peer_id: peer,
					connection_id: conn_id,
					endpoint: &connected,
					failed_addresses: &[],
					other_established: 0usize,
				},
			));
		}

		if let Some(PeerState::Disabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Closed));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn1,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(
			notif.peers.get_mut(&(peer, 0.into())),
			Some(&mut PeerState::Incoming { .. })
		));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn2,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));
	}

	#[test]
	fn two_connections_active_connection_gets_closed_peer_state_is_disabled() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		// open two connections
		for conn_id in vec![conn1, conn2] {
			notif.on_swarm_event(FromSwarm::ConnectionEstablished(
				libp2p::swarm::behaviour::ConnectionEstablished {
					peer_id: peer,
					connection_id: conn_id,
					endpoint: &ConnectedPoint::Listener {
						local_addr: Multiaddr::empty(),
						send_back_addr: Multiaddr::empty(),
					},
					failed_addresses: &[],
					other_established: 0usize,
				},
			));
		}

		if let Some(PeerState::Disabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Closed));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn1,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(
			notif.peers.get_mut(&(peer, 0.into())),
			Some(PeerState::Incoming { .. })
		));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));
	}

	#[test]
	fn inject_connection_closed_for_active_connection() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		// open two connections
		for conn_id in vec![conn1, conn2] {
			notif.on_swarm_event(FromSwarm::ConnectionEstablished(
				libp2p::swarm::behaviour::ConnectionEstablished {
					peer_id: peer,
					connection_id: conn_id,
					endpoint: &connected,
					failed_addresses: &[],
					other_established: 0usize,
				},
			));
		}

		if let Some(PeerState::Disabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Closed));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		// open substreams on both active connections
		notif.peerset_report_connect(peer, set_id);

		if let Some(PeerState::Enabled { connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert_eq!(connections[0], (conn1, ConnectionState::Opening));
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		notif.on_connection_handler_event(
			peer,
			conn1,
			conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]),
		);

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert!(std::matches!(connections[0].1, ConnectionState::Open(_)));
			assert_eq!(connections[0].0, conn1);
			assert_eq!(connections[1], (conn2, ConnectionState::Closed));
		} else {
			panic!("invalid state");
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	fn inject_dial_failure_for_pending_request() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));

		// attempt to connect the backed-off peer and verify that the request is pending
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::PendingRequest { .. })
		));

		let now = Instant::now();
		notif.on_swarm_event(FromSwarm::DialFailure(libp2p::swarm::behaviour::DialFailure {
			peer_id: Some(peer),
			error: &libp2p::swarm::DialError::Aborted,
			connection_id: ConnectionId::new_unchecked(0),
		}));

		if let Some(PeerState::PendingRequest { ref timer_deadline, .. }) =
			notif.peers.get(&(peer, set_id))
		{
			assert!(timer_deadline > &(now + std::time::Duration::from_secs(5)));
		}
	}

	#[test]
	fn peerstate_incoming_open_desired_by_remote() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn1 = ConnectionId::new_unchecked(0);
		let conn2 = ConnectionId::new_unchecked(1);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn1,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn2,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn1,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		// add another open event from remote
		notif.on_connection_handler_event(
			peer,
			conn2,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);

		if let Some(PeerState::Incoming { ref connections, .. }) = notif.peers.get(&(peer, set_id))
		{
			assert_eq!(connections[0], (conn1, ConnectionState::OpenDesiredByRemote));
			assert_eq!(connections[1], (conn2, ConnectionState::OpenDesiredByRemote));
		}
	}

	#[tokio::test]
	async fn remove_backoff_peer_after_timeout() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));

		if let Some(&mut PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, 0.into()))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_millis(100)).unwrap());
		} else {
			panic!("invalid state");
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		let until = if let Some(&PeerState::Backoff { timer_deadline, .. }) =
			notif.peers.get(&(peer, set_id))
		{
			timer_deadline
		} else {
			panic!("invalid state");
		};

		if until > Instant::now() {
			std::thread::sleep(until - Instant::now());
		}

		assert!(notif.peers.get(&(peer, set_id)).is_some());

		if tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				futures::future::poll_fn(|cx| {
					let _ = notif.poll(cx);
					Poll::Ready(())
				})
				.await;

				if notif.peers.get(&(peer, set_id)).is_none() {
					break
				}
			}
		})
		.await
		.is_err()
		{
			panic!("backoff peer was not removed in time");
		}

		assert!(notif.peers.get(&(peer, set_id)).is_none());
	}

	#[tokio::test]
	async fn reschedule_disabled_pending_enable_when_connection_not_closed() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let mut conn_yielder = ConnectionYielder::new();

		// move the peer to `Enabled` state
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &ConnectedPoint::Listener {
					local_addr: Multiaddr::empty(),
					send_back_addr: Multiaddr::empty(),
				},
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// open substream
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		// we rely on the implementation detail that incoming indices are counted from 0
		// to not mock the `Peerset`
		notif.protocol_report_accept(IncomingIndex(0));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		let event = conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]);

		notif.on_connection_handler_event(peer, conn, event);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert!(std::matches!(connections[0], (_, ConnectionState::Open(_))));
			assert_eq!(connections[0].0, conn);
		} else {
			panic!("invalid state");
		}

		notif.peerset_report_disconnect(peer, set_id);

		if let Some(PeerState::Disabled { ref connections, ref mut backoff_until }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			assert!(std::matches!(connections[0], (_, ConnectionState::Closing)));
			assert_eq!(connections[0].0, conn);

			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(2)).unwrap());
		} else {
			panic!("invalid state");
		}

		notif.peerset_report_connect(peer, set_id);

		let prev_instant =
			if let Some(PeerState::DisabledPendingEnable {
				ref connections, timer_deadline, ..
			}) = notif.peers.get(&(peer, set_id))
			{
				assert!(std::matches!(connections[0], (_, ConnectionState::Closing)));
				assert_eq!(connections[0].0, conn);

				*timer_deadline
			} else {
				panic!("invalid state");
			};

		// one of the peers has an active backoff timer so poll the notifications code until
		// the timer has expired. Because the connection is still in the state of `Closing`,
		// verify that the code continues to keep the peer disabled by resetting the timer
		// after the first one expired.
		if tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				futures::future::poll_fn(|cx| {
					let _ = notif.poll(cx);
					Poll::Ready(())
				})
				.await;

				if let Some(PeerState::DisabledPendingEnable {
					timer_deadline, connections, ..
				}) = notif.peers.get(&(peer, set_id))
				{
					assert!(std::matches!(connections[0], (_, ConnectionState::Closing)));

					if timer_deadline != &prev_instant {
						break
					}
				} else {
					panic!("invalid state");
				}
			}
		})
		.await
		.is_err()
		{
			panic!("backoff peer was not removed in time");
		}
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn peerset_report_connect_with_enabled_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		// move the peer to `Enabled` state
		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		let event = conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]);

		notif.on_connection_handler_event(peer, conn, event);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		if let Some(PeerState::Enabled { ref connections, .. }) = notif.peers.get(&(peer, set_id)) {
			assert!(std::matches!(connections[0], (_, ConnectionState::Open(_))));
			assert_eq!(connections[0].0, conn);
		} else {
			panic!("invalid state");
		}

		notif.peerset_report_connect(peer, set_id);
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_connect_with_disabled_pending_enable_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		// switch state to `DisabledPendingEnable`
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));

		// duplicate "connect" must not change the state
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_connect_with_requested_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);

		// Set peer into `Requested` state.
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Requested)));

		// Duplicate "connect" must not change the state.
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Requested)));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_connect_with_pending_requested() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));

		// attempt to connect the backed-off peer and verify that the request is pending
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::PendingRequest { .. })
		));

		// duplicate "connect" must not change the state
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::PendingRequest { .. })
		));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_connect_with_incoming_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_disconnect_with_incoming_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.peerset_report_disconnect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peerset_report_disconnect_with_incoming_peer_protocol_accepts() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		// `Peerset` wants to disconnect the peer but since it's still under validation,
		// it won't be disabled automatically
		notif.peerset_report_disconnect(peer, set_id);

		let incoming_index = match notif.peers.get(&(peer, set_id)) {
			Some(&PeerState::Incoming { peerset_rejected, incoming_index, .. }) => {
				assert!(peerset_rejected);
				incoming_index
			},
			state => panic!("invalid state: {state:?}"),
		};

		// protocol accepted peer but since `Peerset` wanted to disconnect it, the peer will be
		// disabled
		notif.protocol_report_accept(incoming_index);

		match notif.peers.get(&(peer, set_id)) {
			Some(&PeerState::Disabled { .. }) => {},
			state => panic!("invalid state: {state:?}"),
		};
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peer_disconnected_protocol_accepts() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		assert!(notif.incoming.iter().any(|entry| entry.incoming_id == IncomingIndex(0)));
		notif.disconnect_peer(&peer, set_id);

		// since the connection was closed, nothing happens for the peer state because
		// there is nothing actionable
		notif.protocol_report_accept(IncomingIndex(0));

		match notif.peers.get(&(peer, set_id)) {
			Some(&PeerState::Disabled { .. }) => {},
			state => panic!("invalid state: {state:?}"),
		};

		assert!(!notif.incoming.iter().any(|entry| entry.incoming_id == IncomingIndex(0)));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn connection_closed_protocol_accepts() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(0),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		// connection closed, nothing to do
		notif.protocol_report_accept(IncomingIndex(0));

		match notif.peers.get(&(peer, set_id)) {
			None => {},
			state => panic!("invalid state: {state:?}"),
		};
	}

	#[test]
	#[cfg(debug_assertions)]
	fn peer_disconnected_protocol_reject() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		assert!(notif.incoming.iter().any(|entry| entry.incoming_id == IncomingIndex(0)));
		notif.disconnect_peer(&peer, set_id);

		// since the connection was closed, nothing happens for the peer state because
		// there is nothing actionable
		notif.protocol_report_reject(IncomingIndex(0));

		match notif.peers.get(&(peer, set_id)) {
			Some(&PeerState::Disabled { .. }) => {},
			state => panic!("invalid state: {state:?}"),
		};

		assert!(!notif.incoming.iter().any(|entry| entry.incoming_id == IncomingIndex(0)));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn connection_closed_protocol_rejects() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(0),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));

		// connection closed, nothing to do
		notif.protocol_report_reject(IncomingIndex(0));

		match notif.peers.get(&(peer, set_id)) {
			None => {},
			state => panic!("invalid state: {state:?}"),
		};
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn protocol_report_accept_not_incoming_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};
		let mut conn_yielder = ConnectionYielder::new();

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		assert!(std::matches!(
			notif.incoming[0],
			IncomingPeer { alive: true, incoming_id: IncomingIndex(0), .. },
		));

		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		let event = conn_yielder.open_substream(peer, 0, vec![1, 2, 3, 4]);
		notif.on_connection_handler_event(peer, conn, event);

		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));
		notif.incoming[0].alive = true;
		notif.protocol_report_accept(IncomingIndex(0));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_connection_closed_non_existent_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let endpoint = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(0),
				endpoint: &endpoint.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	fn disconnect_non_existent_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let set_id = SetId::from(0);

		notif.peerset_report_disconnect(peer, set_id);

		assert!(notif.peers.is_empty());
		assert!(notif.incoming.is_empty());
	}

	#[test]
	fn accept_non_existent_connection() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		notif.protocol_report_accept(0.into());

		assert!(notif.peers.is_empty());
		assert!(notif.incoming.is_empty());
	}

	#[test]
	fn reject_non_existent_connection() {
		let (mut notif, _controller, _notif_service) = development_notifs();

		notif.protocol_report_reject(0.into());

		assert!(notif.peers.is_empty());
		assert!(notif.incoming.is_empty());
	}

	#[test]
	fn reject_non_active_connection() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.incoming[0].alive = false;
		notif.protocol_report_reject(0.into());

		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_non_existent_connection_closed_for_incoming_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(1337),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_non_existent_connection_closed_for_disabled_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(1337),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_non_existent_connection_closed_for_disabled_pending_enable() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		// switch state to `DisabledPendingEnable`
		notif.peerset_report_connect(peer, set_id);

		assert!(std::matches!(
			notif.peers.get(&(peer, set_id)),
			Some(&PeerState::DisabledPendingEnable { .. })
		));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(1337),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_connection_closed_for_incoming_peer_state_mismatch() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));
		notif.incoming[0].alive = false;

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_connection_closed_for_enabled_state_mismatch() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let set_id = SetId::from(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// remote opens a substream, verify that peer state is updated to `Incoming`
		notif.on_connection_handler_event(
			peer,
			conn,
			NotifsHandlerOut::OpenDesiredByRemote {
				protocol_index: 0,
				handshake: vec![1, 3, 3, 7],
			},
		);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Incoming { .. })));

		// attempt to connect to the peer and verify that the peer state is `Enabled`
		notif.peerset_report_connect(peer, set_id);
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Enabled { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: ConnectionId::new_unchecked(1337),
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn inject_connection_closed_for_backoff_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let set_id = SetId::from(0);
		let peer = PeerId::random();
		let conn = ConnectionId::new_unchecked(0);
		let connected = ConnectedPoint::Listener {
			local_addr: Multiaddr::empty(),
			send_back_addr: Multiaddr::empty(),
		};

		notif.on_swarm_event(FromSwarm::ConnectionEstablished(
			libp2p::swarm::behaviour::ConnectionEstablished {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected,
				failed_addresses: &[],
				other_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Disabled { .. })));

		// manually add backoff for the entry
		if let Some(PeerState::Disabled { ref mut backoff_until, .. }) =
			notif.peers.get_mut(&(peer, set_id))
		{
			*backoff_until =
				Some(Instant::now().checked_add(std::time::Duration::from_secs(5)).unwrap());
		}

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
		assert!(std::matches!(notif.peers.get(&(peer, set_id)), Some(&PeerState::Backoff { .. })));

		notif.on_swarm_event(FromSwarm::ConnectionClosed(
			libp2p::swarm::behaviour::ConnectionClosed {
				peer_id: peer,
				connection_id: conn,
				endpoint: &connected.clone(),
				cause: None,
				remaining_established: 0usize,
			},
		));
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn open_result_ok_non_existent_peer() {
		let (mut notif, _controller, _notif_service) = development_notifs();
		let conn = ConnectionId::new_unchecked(0);
		let mut conn_yielder = ConnectionYielder::new();

		notif.on_connection_handler_event(
			PeerId::random(),
			conn,
			conn_yielder.open_substream(PeerId::random(), 0, vec![1, 2, 3, 4]),
		);
	}
}
