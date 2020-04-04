//! The `sync` module contains an easier to use interface for a connection.
//!
//! It makes it easier to use a connection from multiple threads and use
//! `async`/`await` syntax for the cost of a little bit performance.
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{format_err, Result};
use futures::prelude::*;
use slog::{error, info};
use tokio::sync::{mpsc, oneshot};
use ts_bookkeeping::{ChannelId, TsError};
#[cfg(feature = "audio")]
use tsproto_packets::packets::InAudioBuf;
#[cfg(feature = "unstable")]
use tsproto_packets::packets::OutCommand;

use crate::{events, DisconnectOptions, StreamItem};

enum SyncConMessage {
	RunFn(Box<dyn FnOnce(&mut SyncConnection) + Send>),
	#[cfg(feature = "unstable")]
	SendCommand(OutCommand, oneshot::Sender<std::result::Result<(), TsError>>),
	WaitConnected(oneshot::Sender<Result<()>>),
	Disconnect(DisconnectOptions, oneshot::Sender<Result<()>>),
	DownloadFile {
		channel_id: ChannelId,
		path: String,
		channel_password: Option<String>,
		seek_position: Option<u64>,
		send: oneshot::Sender<Result<super::FileDownloadResult>>,
	},
	UploadFile {
		channel_id: ChannelId,
		path: String,
		channel_password: Option<String>,
		size: u64,
		overwrite: bool,
		resume: bool,
		send: oneshot::Sender<Result<super::FileUploadResult>>,
	},
}

/// This is a subset of [`StreamItem`].
///
/// [`StreamItem`]: ../enum.StreamItem.html
pub enum SyncStreamItem {
	/// All the incoming events.
	///
	/// If a connection to the server was established this will contain an added
	/// event of a server.
	ConEvents(Vec<events::Event>),
	/// Received an audio packet.
	///
	/// Audio packets can be handled by the [`AudioHandler`], which builds a
	/// queue per client and handles packet loss and jitter.
	///
	/// [`AudioHandler`]: audio/structAudioHandler.html
	#[cfg(feature = "audio")]
	Audio(InAudioBuf),
	/// The needed level.
	IdentityLevelIncreasing(u8),
	/// This event may occur without an `IdentityLevelIncreasing` event before
	/// if a new identity is created because no identity was supplied.
	IdentityLevelIncreased,
	/// The connection timed out or the server shut down. The connection will be
	/// rebuilt automatically.
	DisconnectedTemporarily,
}

/// A handle for a [`SyncConnection`] which can be sent across threads.
///
/// All actions like sending messages, downloading and uploading happens through
/// a handle.
///
/// [`SyncConnection`]: struct.SyncConnection.html
#[derive(Clone)]
pub struct SyncConnectionHandle {
	send: mpsc::Sender<SyncConMessage>,
}

pub struct SyncConnection {
	con: super::Connection,
	recv: mpsc::Receiver<SyncConMessage>,
	send: mpsc::Sender<SyncConMessage>,

	commands: HashMap<
		super::MessageHandle,
		oneshot::Sender<std::result::Result<(), TsError>>,
	>,
	connects: Vec<oneshot::Sender<Result<()>>>,
	disconnects: Vec<oneshot::Sender<Result<()>>>,
	downloads: HashMap<
		super::FileTransferHandle,
		oneshot::Sender<Result<super::FileDownloadResult>>,
	>,
	uploads: HashMap<
		super::FileTransferHandle,
		oneshot::Sender<Result<super::FileUploadResult>>,
	>,
}

impl From<super::Connection> for SyncConnection {
	fn from(con: super::Connection) -> Self {
		let (send, recv) = mpsc::channel(1);
		Self {
			con,
			recv,
			send,

			commands: Default::default(),
			connects: Default::default(),
			disconnects: Default::default(),
			downloads: Default::default(),
			uploads: Default::default(),
		}
	}
}

impl Deref for SyncConnection {
	type Target = super::Connection;
	#[inline]
	fn deref(&self) -> &Self::Target { &self.con }
}

impl DerefMut for SyncConnection {
	#[inline]
	fn deref_mut(&mut self) -> &mut <Self as Deref>::Target { &mut self.con }
}

impl Stream for SyncConnection {
	type Item = Result<SyncStreamItem>;
	fn poll_next(
		mut self: Pin<&mut Self>, ctx: &mut Context,
	) -> Poll<Option<Self::Item>> {
		loop {
			if let Poll::Ready(msg) = self.recv.poll_next_unpin(ctx) {
				if let Some(msg) = msg {
					match msg {
						SyncConMessage::RunFn(f) => f(&mut *self),
						#[cfg(feature = "unstable")]
						SyncConMessage::SendCommand(arg, send) => {
							let handle = match self.con.send_command(arg) {
								Ok(r) => r,
								Err(e) => {
									let _ = send.send(Err(e));
									continue;
								}
							};
							self.commands.insert(handle, send);
						}
						SyncConMessage::WaitConnected(send) => {
							if self.con.get_state().is_ok() {
								let _ = send.send(Ok(()));
							} else {
								self.connects.push(send);
							}
						}
						SyncConMessage::Disconnect(arg, send) => {
							match self.con.disconnect(arg) {
								Ok(r) => r,
								Err(e) => {
									let _ = send.send(Err(e));
									continue;
								}
							}
							self.disconnects.push(send);
						}
						SyncConMessage::DownloadFile {
							channel_id,
							path,
							channel_password,
							seek_position,
							send,
						} => {
							let handle = match self.con.download_file(
								channel_id,
								&path,
								channel_password.as_ref().map(|s| s.as_str()),
								seek_position,
							) {
								Ok(r) => r,
								Err(e) => {
									let _ = send.send(Err(e));
									continue;
								}
							};
							self.downloads.insert(handle, send);
						}
						SyncConMessage::UploadFile {
							channel_id,
							path,
							channel_password,
							size,
							overwrite,
							resume,
							send,
						} => {
							let handle = match self.con.upload_file(
								channel_id,
								&path,
								channel_password.as_ref().map(|s| s.as_str()),
								size,
								overwrite,
								resume,
							) {
								Ok(r) => r,
								Err(e) => {
									let _ = send.send(Err(e));
									continue;
								}
							};
							self.uploads.insert(handle, send);
						}
					}
					continue;
				} else {
					error!(
						self.con.logger,
						"Message stream ended unexpectedly"
					);
				}
			}
			break;
		}

		loop {
			break if let Poll::Ready(item) = self.con.poll_next(ctx) {
				Poll::Ready(match item {
					Some(Ok(item)) => Some(Ok(match item {
						StreamItem::ConEvents(i) => {
							self.connects.drain(..).for_each(|send| {
								let _ = send.send(Ok(()));
							});
							SyncStreamItem::ConEvents(i)
						}
						#[cfg(feature = "audio")]
						StreamItem::Audio(i) => SyncStreamItem::Audio(i),
						StreamItem::IdentityLevelIncreasing(i) => {
							SyncStreamItem::IdentityLevelIncreasing(i)
						}
						StreamItem::IdentityLevelIncreased => {
							SyncStreamItem::IdentityLevelIncreased
						}
						StreamItem::DisconnectedTemporarily => {
							SyncStreamItem::DisconnectedTemporarily
						}
						StreamItem::MessageResult(handle, res) => {
							if let Some(send) = self.commands.remove(&handle) {
								let _ = send.send(res);
							} else {
								info!(
									self.con.logger,
									"Got untracked message result"
								);
							}
							continue;
						}
						StreamItem::FileDownload(handle, res) => {
							if let Some(send) = self.downloads.remove(&handle) {
								let _ = send.send(Ok(res));
							} else {
								info!(
									self.con.logger,
									"Got untracked download"
								);
							}
							continue;
						}
						StreamItem::FileUpload(handle, res) => {
							if let Some(send) = self.uploads.remove(&handle) {
								let _ = send.send(Ok(res));
							} else {
								info!(self.con.logger, "Got untracked upload");
							}
							continue;
						}
						StreamItem::FileTransferFailed(handle, res) => {
							if let Some(send) = self.downloads.remove(&handle) {
								let _ = send.send(Err(res));
							} else if let Some(send) =
								self.uploads.remove(&handle)
							{
								let _ = send.send(Err(res));
							} else {
								info!(
									self.con.logger,
									"Got untracked file transfer"
								);
							}
							continue;
						}
					})),
					Some(Err(e)) => Some(Err(e)),
					None => {
						self.disconnects.drain(..).for_each(|send| {
							let _ = send.send(Ok(()));
						});
						None
					}
				})
			} else {
				Poll::Pending
			};
		}
	}
}

impl SyncConnection {
	/// Get a handle to the connection that can be sent across threads.
	#[inline]
	pub fn get_handle(&self) -> SyncConnectionHandle {
		SyncConnectionHandle { send: self.send.clone() }
	}
}

impl SyncConnectionHandle {
	/// Run a function on the connection.
	pub async fn with_connection<
		T: Send + 'static,
		F: FnOnce(&mut SyncConnection) -> T + Send + 'static,
	>(
		&mut self, f: F,
	) -> Result<T> {
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::RunFn(Box::new(move |con| {
				let _ = send.send(f(con));
			})))
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await?)
	}

	/// Adds a `return_code` to the command and returns if the corresponding
	/// answer is received. If an error occurs, the future will return an error.
	#[cfg(feature = "unstable")]
	pub async fn send_command(&mut self, arg: OutCommand) -> Result<()> {
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::SendCommand(arg, send))
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await??)
	}

	/// This future resolves once the connection is connected to the server.
	pub async fn wait_until_connected(&mut self) -> Result<()> {
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::WaitConnected(send))
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await??)
	}

	/// Disconnect from the server.
	///
	/// # Arguments
	/// - `options`: Either `None` or `DisconnectOptions`.
	///
	/// # Examples
	///
	/// Use default options:
	///
	/// ```no_run
	/// # use futures::prelude::*;
	/// # use tsclientlib::{Connection, ConnectOptions, DisconnectOptions, StreamItem};
	/// # use tsclientlib::sync::SyncConnection;
	///
	/// # #[tokio::main]
	/// # async fn main() {
	/// let con: SyncConnection = Connection::new(ConnectOptions::new("localhost")).unwrap().into();
	/// let mut handle = con.get_handle();
	/// tokio::spawn(con.for_each(|_| future::ready(())));
	/// // Wait until connected
	/// handle.wait_until_connected().await.unwrap();
	///
	/// // Disconnect
	/// handle.disconnect(DisconnectOptions::new()).await.unwrap();
	/// # }
	/// ```
	///
	/// Specify a reason and a quit message:
	///
	/// ```no_run
	/// # use futures::prelude::*;
	/// # use tsclientlib::{Connection, ConnectOptions, DisconnectOptions, Reason, StreamItem};
	/// # use tsclientlib::sync::SyncConnection;
	///
	/// # #[tokio::main]
	/// # async fn main() {
	/// let con: SyncConnection = Connection::new(ConnectOptions::new("localhost")).unwrap().into();
	/// let mut handle = con.get_handle();
	/// tokio::spawn(con.for_each(|_| future::ready(())));
	/// // Wait until connected
	/// handle.wait_until_connected().await.unwrap();
	///
	/// // Disconnect
	/// let options = DisconnectOptions::new()
	///     .reason(Reason::Clientdisconnect)
	///     .message("Away for a while");
	/// handle.disconnect(DisconnectOptions::new()).await.unwrap();
	/// # }
	/// ```
	pub async fn disconnect(&mut self, arg: DisconnectOptions) -> Result<()> {
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::Disconnect(arg, send))
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await??)
	}

	/// Download a file from a channel of the connected TeamSpeak server.
	///
	/// Returns the size of the file and a tcp stream of the requested file.
	///
	/// # Example
	/// Download an icon.
	///
	/// ```no_run
	/// # use tsclientlib::ChannelId;
	/// # let handle: tsclientlib::sync::SyncConnectionHandle = panic!();
	/// # let id = 0;
	/// let download = handle.download_file(ChannelId(0), format!("/icon_{}", id), None, None);
	/// ```
	pub async fn download_file(
		&mut self, channel_id: ChannelId, path: String,
		channel_password: Option<String>, seek_position: Option<u64>,
	) -> Result<super::FileDownloadResult>
	{
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::DownloadFile {
				channel_id,
				path,
				channel_password,
				seek_position,
				send,
			})
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await??)
	}

	/// Upload a file to a channel of the connected TeamSpeak server.
	///
	/// Returns the size of the part which is already uploaded (when resume is
	/// specified) and a tcp stream where the requested file should be uploaded.
	///
	/// # Example
	/// Upload an avatar.
	///
	/// ```no_run
	/// # use tsclientlib::ChannelId;
	/// # let handle: tsclientlib::sync::SyncConnectionHandle = panic!();
	/// # let size = 0;
	/// let upload = handle.upload_file(ChannelId(0), "/avatar".to_string(), None, size, true, false);
	/// ```
	pub async fn upload_file(
		&mut self, channel_id: ChannelId, path: String,
		channel_password: Option<String>, size: u64, overwrite: bool,
		resume: bool,
	) -> Result<super::FileUploadResult>
	{
		let (send, recv) = oneshot::channel();
		self.send
			.send(SyncConMessage::UploadFile {
				channel_id,
				path,
				channel_password,
				size,
				overwrite,
				resume,
				send,
			})
			.await
			.map_err(|_| format_err!("Connection has gone"))?;
		Ok(recv.await??)
	}
}
