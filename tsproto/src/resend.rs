use std::cmp::{Ord, Ordering};
use std::collections::{BTreeMap, BinaryHeap};
use std::convert::From;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Sub};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::bail;
use futures::prelude::*;
use num_traits::ToPrimitive;
use slog::{info, warn, Logger};
use tokio::time::{Delay, Duration, Instant};
use tsproto_packets::packets::*;

use crate::connection::{Connection, StreamItem};
use crate::{Result, UDP_SINK_CAPACITY};

// TODO implement fast retransmit: 2 Acks received but earlier packet not acked -> retransmit
// TODO implement slow start and redo slow start when send window reaches 1, also reset all tries then

// Use cubic for congestion control: https://en.wikipedia.org/wiki/CUBIC_TCP
// But scaling with number of sent packets instead of time because we might not
// send packets that often.

/// Congestion windows gets down to 0.3*w_max for BETA=0.7
const BETA: f32 = 0.7;
/// Increase over w_max after roughly 5 packets (C=0.2 needs seven packets).
const C: f32 = 0.5;

/// Events to inform a resender of the current state of a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ResenderState {
	/// The connection is starting, reduce the timeout time.
	Connecting,
	/// The handshake is completed, this is the normal operation mode.
	Connected,
	/// The connection is tearing down, reduce the timeout time.
	Disconnecting,
	/// The connection is gone, we only send ack packets.
	Disconnected,
}

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct PartialPacketId {
	pub generation_id: u32,
	pub packet_id: u16,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PacketId {
	pub packet_type: PacketType,
	pub part: PartialPacketId,
}

#[derive(Clone, Debug)]
pub struct SendRecordId {
	/// The last time when the packet was sent.
	pub last: Instant,
	/// How often the packet was already resent.
	pub tries: usize,
	id: PacketId,
}

/// A record of a packet that can be resent.
#[derive(Debug)]
struct SendRecord {
	/// When this packet was originally sent.
	pub sent: Instant,
	pub id: SendRecordId,
	pub packet: OutUdpPacket,
}

/// Resend command and init packets until the other side acknowledges them.
#[derive(Debug)]
pub struct Resender {
	/// Send queue ordered by when a packet has to be sent.
	///
	/// The maximum in this queue is the next packet that should be resent.
	/// This is a part of `full_send_queue`.
	send_queue: BinaryHeap<SendRecordId>,
	/// Send queue ordered by packet id.
	///
	/// There is one queue per packet type: `Init`, `Command` and `CommandLow`.
	full_send_queue: [BTreeMap<PartialPacketId, SendRecord>; 3],
	/// All packets with an id less than this index id are currently in the
	/// `send_queue`. Packets with an id greater or equal to this index are not
	/// in the send queue.
	send_queue_indices: [PartialPacketId; 3],
	config: ResendConfig,
	state: ResenderState,

	// Congestion control
	/// The maximum send window before the last reduction.
	w_max: u16,
	/// The time when the last packet loss occured.
	///
	/// This is not necessarily the accurate time, but the duration until
	/// now/no_congestion_since is accurate.
	last_loss: Instant,
	/// The send queue was never full since this time. We use this to not
	/// increase the send window in this case.
	no_congestion_since: Option<Instant>,

	/// When the last packet was added to the send queue or received.
	///
	/// This is used to decide when to send ping packets.
	last_receive: Instant,
	/// When the last packet was added to the send queue.
	///
	/// This is used to handle timeouts when disconnecting.
	last_send: Instant,

	/// The future to wake us up when the next packet should be resent.
	timeout: Delay,
	/// The timer used for sending ping packets.
	ping_timeout: Delay,
	/// The timer used for disconnecting the connection.
	state_timeout: Delay,
}

#[derive(Clone, Debug)]
pub struct ResendConfig {
	// Close the connection after no packet is received for this duration.
	pub connecting_timeout: Duration,
	pub normal_timeout: Duration,
	pub disconnect_timeout: Duration,

	/// Start value for the Smoothed Round Trip Time.
	pub srtt: Duration,
	/// Start value for the deviation of the srtt.
	pub srtt_dev: Duration,
}

impl Ord for PartialPacketId {
	fn cmp(&self, other: &Self) -> Ordering {
		self.generation_id
			.cmp(&other.generation_id)
			.then_with(|| self.packet_id.cmp(&other.packet_id))
	}
}

impl PartialOrd for PartialPacketId {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Add<u16> for PartialPacketId {
	type Output = Self;
	fn add(self, rhs: u16) -> Self::Output {
		let (packet_id, next_gen) = self.packet_id.overflowing_add(rhs);
		Self {
			generation_id: if next_gen {
				self.generation_id.wrapping_add(1)
			} else {
				self.generation_id
			},
			packet_id,
		}
	}
}

impl Sub<u16> for PartialPacketId {
	type Output = Self;
	fn sub(self, rhs: u16) -> Self::Output {
		let (packet_id, last_gen) = self.packet_id.overflowing_sub(rhs);
		Self {
			generation_id: if last_gen {
				self.generation_id.wrapping_sub(1)
			} else {
				self.generation_id
			},
			packet_id,
		}
	}
}

impl PartialOrd for PacketId {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		if self.packet_type == other.packet_type {
			Some(self.part.cmp(&other.part))
		} else {
			None
		}
	}
}

impl From<&OutUdpPacket> for PacketId {
	fn from(packet: &OutUdpPacket) -> Self {
		Self {
			packet_type: packet.packet_type(),
			part: PartialPacketId {
				generation_id: packet.generation_id(),
				packet_id: packet.packet_id(),
			},
		}
	}
}

impl fmt::Debug for PacketId {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{:?}{:?}", self.packet_type, self.part)?;
		Ok(())
	}
}

impl fmt::Debug for PartialPacketId {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "({:x}:{:x})", self.generation_id, self.packet_id)?;
		Ok(())
	}
}

impl Ord for SendRecordId {
	fn cmp(&self, other: &Self) -> Ordering {
		// If the packet was not already sent, it is more important
		if self.tries == 0 {
			if other.tries != 0 {
				return Ordering::Greater;
			}
		} else if other.tries == 0 {
			return Ordering::Less;
		}
		// The smallest time is the most important time
		self.last.cmp(&other.last).reverse().then_with(||
			// Else, the lower packet id is more important
			self.id.part.cmp(&other.id.part).reverse())
	}
}

impl PartialOrd for SendRecordId {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl PartialEq for SendRecordId {
	fn eq(&self, other: &Self) -> bool { self.id.eq(&other.id) }
}
impl Eq for SendRecordId {}

impl Hash for SendRecordId {
	fn hash<H: Hasher>(&self, state: &mut H) { self.id.hash(state); }
}

impl Default for Resender {
	fn default() -> Self {
		let now = Instant::now();
		Self {
			send_queue: Default::default(),
			full_send_queue: Default::default(),
			send_queue_indices: Default::default(),
			config: Default::default(),
			state: ResenderState::Connecting,

			w_max: UDP_SINK_CAPACITY as u16,
			last_loss: now,
			no_congestion_since: Some(now),

			timeout: tokio::time::delay_for(std::time::Duration::from_secs(1)),
			last_receive: now,
			last_send: now,
			ping_timeout: tokio::time::delay_for(
				std::time::Duration::from_secs(1),
			),
			state_timeout: tokio::time::delay_for(
				std::time::Duration::from_secs(1),
			),
		}
	}
}

impl Resender {
	fn packet_type_to_index(t: PacketType) -> usize {
		match t {
			PacketType::Init => 0,
			PacketType::Command => 1,
			PacketType::CommandLow => 2,
			_ => panic!("Resender cannot handle packet type {:?}", t),
		}
	}

	pub fn ack_packet(
		con: &mut Connection, cx: &mut Context, p_type: PacketType, p_id: u16,
	) {
		// Remove from ordered queue
		let queue = &mut con.resender.full_send_queue
			[Self::packet_type_to_index(p_type)];
		let mut queue_iter = queue.iter();
		if let Some((first, _)) = queue_iter.next() {
			let id = if first.packet_id == p_id {
				let (gen, p_id) = if let Some((_, rec2)) = queue_iter.next() {
					// Ack all until the next packet
					let rec2_id = &rec2.id.id.part;
					(rec2_id.generation_id, rec2_id.packet_id)
				} else if p_type == PacketType::Init {
					// Ack the current packet
					(0, p_id + 1)
				} else {
					// Ack all until the next packet to send
					con.codec.outgoing_p_ids[p_type.to_usize().unwrap()]
				};

				let (p_id, last_gen) = p_id.overflowing_sub(1);
				let id = PacketId {
					packet_type: p_type,
					part: PartialPacketId {
						generation_id: if last_gen {
							gen.wrapping_sub(1)
						} else {
							gen
						},
						packet_id: p_id,
					},
				};
				con.stream_items.push_back(StreamItem::AckPacket(id));

				first.clone()
			} else {
				PartialPacketId {
					generation_id: if p_id < first.packet_id {
						first.generation_id.wrapping_add(1)
					} else {
						first.generation_id
					},
					packet_id: p_id,
				}
			};

			if let Some(rec) = queue.remove(&id) {
				// Update srtt if the packet was not resent
				if rec.id.tries == 1 {
					let now = Instant::now();
					con.resender.update_srtt(now - rec.sent);
				}

				// Notify the waker that we can send another packet from the
				// send queue.
				cx.waker().wake_by_ref();
			}
		}
	}

	pub fn received_packet(&mut self) { self.last_receive = Instant::now(); }

	fn get_timeout(&self) -> Duration {
		match self.state {
			ResenderState::Connecting => self.config.connecting_timeout,
			ResenderState::Disconnecting | ResenderState::Disconnected => {
				self.config.disconnect_timeout
			}
			ResenderState::Connected => self.config.normal_timeout,
		}
	}

	/// Inform the resender of state changes of the connection.
	pub fn set_state(&mut self, logger: &Logger, state: ResenderState) {
		info!(logger, "Resender: Changed state"; "from" => ?self.state,
			"to" => ?state);
		self.state = state;

		self.last_send = Instant::now();
		self.state_timeout.reset(self.last_send + self.get_timeout());
	}

	pub fn get_state(&self) -> ResenderState { self.state }

	/// If the send queue is full if it reached the congestion window size or
	/// it contains packets that were not yet sent once.
	pub fn is_full(&self) -> bool {
		self.full_send_queue.len() >= self.get_window() as usize
	}

	/// If the send queue is empty.
	pub fn is_empty(&self) -> bool { self.send_queue.is_empty() }

	/// Take the first packets from `to_send_ordered` and put them into
	/// `to_send`.
	///
	/// This is done on packet loss, when the send queue is rebuilt.
	fn rebuild_send_queue(&mut self) {
		self.send_queue.clear();
		self.send_queue_indices = Default::default();
		self.fill_up_send_queue();
	}

	/// Fill up to the send window size.
	fn fill_up_send_queue(&mut self) {
		let get_skip_closure = |i: usize| {
			let start = self.send_queue_indices[i].clone();
			move |r: &&SendRecord| r.id.id.part < start
		};
		let mut iters = [
			self.full_send_queue[0]
				.values()
				.skip_while(get_skip_closure(0))
				.peekable(),
			self.full_send_queue[1]
				.values()
				.skip_while(get_skip_closure(1))
				.peekable(),
			self.full_send_queue[2]
				.values()
				.skip_while(get_skip_closure(2))
				.peekable(),
		];

		for _ in self.send_queue.len()..(self.get_window() as usize) {
			let mut max_i = None;
			let mut min_time = None;

			for (i, iter) in iters.iter_mut().enumerate() {
				if let Some(rec) = iter.peek() {
					if min_time.map(|t| t < rec.sent).unwrap_or(true) {
						max_i = Some(i);
						min_time = Some(rec.sent);
					}
				}
			}

			if let Some(max_i) = max_i {
				let max = iters[max_i].next().unwrap().id.clone();
				self.send_queue_indices[max_i] = max.id.part + 1;
				self.send_queue.push(max);
			} else {
				if self.no_congestion_since.is_none() {
					self.no_congestion_since = Some(Instant::now());
				}
				return;
			}
		}

		if let Some(until) = self.no_congestion_since.take() {
			self.last_loss = Instant::now() - (until - self.last_loss);
		}
	}

	/// The amount of packets that can be in-flight concurrently.
	///
	/// The CUBIC congestion control window.
	fn get_window(&self) -> u16 {
		let time = self.no_congestion_since.unwrap_or_else(|| Instant::now())
			- self.last_loss;
		let res = C
			* (time.as_secs_f32()
				- (self.w_max as f32 * BETA / C).powf(1.0 / 3.0))
			.powf(3.0) + self.w_max as f32;
		let max = u16::max_value() / 2;
		if res > max as f32 {
			max
		} else if res < 1.0 {
			1
		} else {
			res as u16
		}
	}

	/// Add another duration to the stored smoothed rtt.
	fn update_srtt(&mut self, rtt: Duration) {
		let diff = if rtt > self.config.srtt {
			rtt - self.config.srtt
		} else {
			self.config.srtt - rtt
		};
		self.config.srtt_dev = self.config.srtt_dev * 3 / 4 + diff / 4;
		self.config.srtt = self.config.srtt * 7 / 8 + rtt / 8;
	}

	pub fn send_packet(con: &mut Connection, packet: OutUdpPacket) {
		con.resender.last_send = Instant::now();
		let rec = SendRecord {
			sent: Instant::now(),
			id: SendRecordId {
				last: Instant::now(),
				tries: 0,
				id: (&packet).into(),
			},
			packet,
		};

		let i = Self::packet_type_to_index(rec.id.id.packet_type);
		con.resender.full_send_queue[i].insert(rec.id.id.part.clone(), rec);
		con.resender.fill_up_send_queue();
	}

	/// Returns an error if the timeout is exceeded and the connection is
	/// considered dead or another unrecoverable error occurs.
	pub fn poll_resend(con: &mut Connection, cx: &mut Context) -> Result<()> {
		let timeout = con.resender.get_timeout();
		// Send a packet at least every second
		let max_send_rto = Duration::from_secs(1);

		// Check if there are packets to send.
		loop {
			let now = Instant::now();
			let window = con.resender.get_window();

			// Retransmission timeout
			let mut rto: Duration =
				con.resender.config.srtt + con.resender.config.srtt_dev * 4;
			if rto > max_send_rto {
				rto = max_send_rto;
			}
			let last_threshold = now - rto;

			let mut rec = if let Some(rec) = con.resender.send_queue.peek_mut()
			{
				rec
			} else {
				break;
			};

			// Skip if not contained in full_send_queue. This happens when the
			// packet was acknowledged.
			let full_queue = &mut con.resender.full_send_queue
				[Self::packet_type_to_index(rec.id.packet_type)];
			let full_rec = if let Some(r) = full_queue.get_mut(&rec.id.part) {
				r
			} else {
				drop(rec);
				con.resender.send_queue.pop();
				con.resender.fill_up_send_queue();
				continue;
			};

			// Check if we should resend this packet or not
			if rec.tries != 0 && rec.last > last_threshold {
				// Schedule next send
				let dur = rec.last - last_threshold;
				con.resender.timeout.reset(now + dur);
				if let Poll::Ready(()) = con.resender.timeout.poll_unpin(cx) {
					continue;
				}
				break;
			}

			if now - full_rec.sent > timeout {
				bail!("Connection timed out");
			}

			// Try to send this packet
			match Connection::static_poll_send_udp_packet(
				&*con.udp_socket,
				&con.address,
				&con.event_listeners,
				cx,
				&full_rec.packet,
			) {
				Poll::Pending => break,
				Poll::Ready(r) => {
					r?;
					if rec.tries != 0 {
						let to_s = if con.is_client { "S" } else { "C" };
						warn!(con.logger, "Resend";
							"id" => ?rec.id,
							"tries" => rec.tries,
							"last" => format!("{:?} ago", now - rec.last),
							"to" => to_s,
							"srtt" => ?con.resender.config.srtt,
							"srtt_dev" => ?con.resender.config.srtt_dev,
							"rto" => ?rto,
							"send_window" => window,
						);
					}

					// Successfully started sending the packet, update record
					rec.last = now;
					rec.tries += 1;
					full_rec.id = rec.clone();

					if rec.tries != 1 {
						drop(rec);
						// Double srtt on packet loss
						con.resender.config.srtt = con.resender.config.srtt * 2;
						if con.resender.config.srtt > timeout {
							con.resender.config.srtt = timeout;
						}

						// Handle congestion window
						con.resender.w_max = con.resender.get_window();
						con.resender.last_loss = Instant::now();
						con.resender.no_congestion_since = None;
						con.resender.rebuild_send_queue();
					}
				}
			}
		}

		Ok(())
	}

	/// Returns an error if the timeout is exceeded and the connection is
	/// considered dead or another unrecoverable error occurs.
	pub fn poll_ping(con: &mut Connection, cx: &mut Context) -> Result<()> {
		let now = Instant::now();
		let timeout = con.resender.get_timeout();

		if con.resender.state == ResenderState::Disconnecting {
			if now - con.resender.last_send >= timeout {
				bail!("Connection timed out");
			}

			con.resender.state_timeout.reset(con.resender.last_send + timeout);
			if let Poll::Ready(()) =
				Pin::new(&mut con.resender.state_timeout).poll(cx)
			{
				bail!("Connection timed out");
			}
		}

		// TODO Send ping packets if needed
		Ok(())
	}
}

impl Default for ResendConfig {
	fn default() -> Self {
		Self {
			connecting_timeout: Duration::from_secs(5),
			normal_timeout: Duration::from_secs(30),
			disconnect_timeout: Duration::from_secs(5),

			srtt: Duration::from_millis(500),
			srtt_dev: Duration::from_millis(0),
		}
	}
}
