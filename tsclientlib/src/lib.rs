//! tsclientlib is a library which makes it simple to create TeamSpeak clients
//! and bots.
//!
//! If you want a full client application, you might want to have a look at
//! [Qint].
//!
//! The base class of this library is the [`Connection`]. One instance of this
//! struct manages a single connection to a server.
//!
//! [`Connection`]: struct.Connection.html
//! [Qint]: https://github.com/ReSpeak/Qint

// TODO Update
// # Internal structure
// ConnectionManager is a wrapper around Rc<RefCell<InnerCM>>,
// it contains the api to create and destroy connections.
// To inspect/modify things, facade objects are used (included in lib.rs).
// All facade objects exist in a mutable and non-mutable version and they borrow
// the ConnectionManager.
// That means all references have to be handed back before the ConnectionManager
// can be used again.
//
// InnerCM contains a HashMap<ConnectionId, structs::NetworkWrapper>.
//
// NetworkWrapper contains the Connection which is the bookkeeping struct,
// the Rc<RefCell<client::ClientData>>, which is the tsproto connection,
// a reference to the ClientConnection of tsproto
// and a stream of Messages.
//
// The NetworkWrapper wraps the stream of Messages and updates the
// bookkeeping, raises events, etc. on new packets.
// The ConnectionManager wraps all those streams into one stream (like a
// select). To progress, the user of the library has to poll the
// ConnectionManager for new notifications and sound.
//
// The items of the stream are either Messages or audio data.

// TODO
#![allow(dead_code)]

extern crate base64;
extern crate chrono;
#[macro_use]
extern crate failure;
extern crate futures;
extern crate rand;
extern crate reqwest;
#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_perf;
extern crate slog_term;
extern crate tokio;
extern crate trust_dns_proto;
extern crate trust_dns_resolver;
extern crate tsproto;
extern crate tsproto_commands;

use std::fmt;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard, Once, ONCE_INIT};

use chrono::{DateTime, Utc};
use failure::ResultExt;
use futures::{future, Future, Sink, stream, Stream};
use futures::sync::mpsc;
use slog::{Drain, Logger};
use tsproto::algorithms as algs;
use tsproto::{client, crypto, packets, commands};
use tsproto::commands::Command;
use tsproto::handler_data::ConnectionValue;
use tsproto::packets::{Header, Packet, PacketType};
use tsproto_commands::*;

macro_rules! copy_attrs {
    ($from:ident, $to:ident; $($attr:ident),* $(,)*; $($extra:ident: $ex:expr),* $(,)*) => {
        $to {
            $($attr: $from.$attr.clone(),)*
            $($extra: $ex,)*
        }
    };
}

/*macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(e) => e,
            Err(error) => return Box::new(future::err(error.into())),
        }
    };
}*/

pub mod codec;
pub mod data;
pub mod resolver;

// Reexports
pub use tsproto_commands::Reason;
pub use tsproto_commands::versions::Version;
use tsproto_commands::messages;

use codec::Message;

type BoxFuture<T> = Box<Future<Item = T, Error = Error> + Send>;
type Result<T> = std::result::Result<T, Error>;

include!(concat!(env!("OUT_DIR"), "/getters.rs"));

#[derive(Fail, Debug)]
pub enum Error {
    #[fail(display = "{}", _0)]
    Base64(#[cause] base64::DecodeError),
    #[fail(display = "{}", _0)]
    Canceled(#[cause] futures::Canceled),
    #[fail(display = "{}", _0)]
    DnsProto(#[cause] trust_dns_proto::error::ProtoError),
    #[fail(display = "{}", _0)]
    Io(#[cause] std::io::Error),
    #[fail(display = "{}", _0)]
    ParseMessage(#[cause] tsproto_commands::messages::ParseError),
    #[fail(display = "{}", _0)]
    Resolve(#[cause] trust_dns_resolver::error::ResolveError),
    #[fail(display = "{}", _0)]
    Reqwest(#[cause] reqwest::Error),
    #[fail(display = "{}", _0)]
    Tsproto(#[cause] tsproto::Error),
    #[fail(display = "{}", _0)]
    Utf8(#[cause] std::str::Utf8Error),
    #[fail(display = "{}", _0)]
    Other(#[cause] failure::Compat<failure::Error>),

    #[fail(display = "Connection failed ({})", _0)]
    ConnectionFailed(String),

    #[doc(hidden)]
    #[fail(display = "Nonexhaustive enum – not an error")]
    __Nonexhaustive,
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::Base64(e)
    }
}

impl From<futures::Canceled> for Error {
    fn from(e: futures::Canceled) -> Self {
        Error::Canceled(e)
    }
}

impl From<trust_dns_proto::error::ProtoError> for Error {
    fn from(e: trust_dns_proto::error::ProtoError) -> Self {
        Error::DnsProto(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<tsproto_commands::messages::ParseError> for Error {
    fn from(e: tsproto_commands::messages::ParseError) -> Self {
        Error::ParseMessage(e)
    }
}

impl From<trust_dns_resolver::error::ResolveError> for Error {
    fn from(e: trust_dns_resolver::error::ResolveError) -> Self {
        Error::Resolve(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Reqwest(e)
    }
}

impl From<tsproto::Error> for Error {
    fn from(e: tsproto::Error) -> Self {
        Error::Tsproto(e)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Utf8(e)
    }
}

impl From<failure::Error> for Error {
    fn from(e: failure::Error) -> Self {
        let r: std::result::Result<(), _> = Err(e);
        Error::Other(r.compat().unwrap_err())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ChannelType {
    Permanent,
    SemiPermanent,
    Temporary,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum MaxFamilyClients {
    Unlimited,
    Inherited,
    Limited(u16),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TalkPowerRequest {
    pub time: DateTime<Utc>,
    pub message: String,
}

struct SimplePacketHandler {
    logger: Logger,
    handle_packets: Option<PHBox>,
    initserver_sender: Option<mpsc::Sender<Command>>,
}

impl SimplePacketHandler {
    fn new(logger: Logger) -> Self {
        Self { logger, handle_packets: None, initserver_sender: None }
    }
}

impl<T: 'static> tsproto::handler_data::PacketHandler<T> for
    SimplePacketHandler {
    fn new_connection<S1, S2>(
        &mut self,
        _: &ConnectionValue<T>,
        command_stream: S1,
        audio_stream: S2,
    ) where
        S1: Stream<Item=Packet, Error=tsproto::Error> + Send + 'static,
        S2: Stream<Item=Packet, Error=tsproto::Error> + Send + 'static,
    {
        let command_stream: Box<Stream<Item=Packet, Error=tsproto::Error>
            + Send> = if let Some(send) = &self.initserver_sender {
            let mut send = send.clone();
            Box::new(command_stream.map(move |p| {
                let is_cmd = if let Packet { data: packets::Data::Command(_), .. } = &p {
                    true
                } else {
                    false
                };
                if is_cmd {
                    if let Packet { data: packets::Data::Command(cmd), .. }
                        = p {
                        // Don't block, we should only send 1 command
                        let _ = send.try_send(cmd);
                        None
                    } else {
                        unreachable!();
                    }
                } else {
                    Some(p)
                }
            }).filter_map(|p| p))
        } else {
            Box::new(command_stream)
        };

        if let Some(h) = &mut self.handle_packets {
            h.new_connection(Box::new(command_stream), Box::new(audio_stream));
        } else {
            let logger = self.logger.clone();
            tokio::spawn(command_stream.for_each(|_| Ok(())).map_err(move |e|
                error!(logger, "Command stream exited with error ({:?})", e)));
            let logger = self.logger.clone();
            tokio::spawn(audio_stream.for_each(|_| Ok(())).map_err(move |e|
                error!(logger, "Audio stream exited with error ({:?})", e)));
        }
    }
}

type PHBox = Box<PacketHandler + Send>;
pub trait PacketHandler {
    fn new_connection(
        &mut self,
        command_stream: Box<Stream<Item=Packet, Error=tsproto::Error> + Send>,
        audio_stream: Box<Stream<Item=Packet, Error=tsproto::Error> + Send>,
    );
    /// Clone into a box.
    fn clone(&self) -> PHBox;
}

pub struct ConnectionLock<'a> {
    guard: MutexGuard<'a, data::Connection>,
}

impl<'a> Deref for ConnectionLock<'a> {
    type Target = data::Connection;

    fn deref(&self) -> &Self::Target {
        &*self.guard
    }
}

#[derive(Clone)]
struct InnerConnection {
    connection: Arc<Mutex<data::Connection>>,
    client_data: client::ClientDataM<SimplePacketHandler>,
    client_connection: client::ClientConVal,
}

#[derive(Clone)]
pub struct Connection {
    inner: InnerConnection,
}

impl Connection {
    pub fn new(mut options: ConnectOptions) -> BoxFuture<Connection> {
        // Initialize tsproto if it was not done yet
        static TSPROTO_INIT: Once = ONCE_INIT;
        TSPROTO_INIT.call_once(|| tsproto::init()
            .expect("tsproto failed to initialize"));

        let logger = options.logger.take().unwrap_or_else(|| {
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator).build().fuse();
            let drain = slog_async::Async::new(drain).build().fuse();

            slog::Logger::root(drain, o!())
        });
        let logger = logger.new(o!("addr" => options.address.to_string()));

        // Try all addresses
        let addr: Box<Stream<Item=_, Error=_> + Send> = options.address.resolve(&logger);
        let private_key = match options.private_key.take().map(Ok)
            .unwrap_or_else(|| {
                // Create new ECDH key
                crypto::EccKeyPrivP256::create()
            }) {
            Ok(key) => key,
            Err(e) => return Box::new(future::err(e.into())),
        };

        let logger2 = logger.clone();
        Box::new(addr.and_then(move |addr| -> Box<Future<Item=_, Error=_> + Send> {
            let log_config = tsproto::handler_data::LogConfig::new(
                options.log_packets, options.log_packets);
            let mut packet_handler = SimplePacketHandler::new(logger.clone());
            let (initserver_send, initserver_recv) = mpsc::channel(0);
            packet_handler.initserver_sender = Some(initserver_send);
            if let Some(h) = &options.handle_packets {
                packet_handler.handle_packets = Some(h.as_ref().clone());
            }
            let packet_handler = client::DefaultPacketHandler::new(
                packet_handler);
            let client = match client::ClientData::new(
                options.local_address.unwrap_or_else(|| if addr.is_ipv4() {
                    "0.0.0.0:0".parse().unwrap()
                } else {
                    "[::]:0".parse().unwrap()
                }),
                private_key.clone(),
                true,
                None,
                packet_handler,
                tsproto::connectionmanager::SocketConnectionManager::new(),
                logger.clone(),
                log_config,
            ) {
                Ok(client) => client,
                Err(error) => return Box::new(future::err(error.into())),
            };

            // Set the data reference
            let client2 = Arc::downgrade(&client);
            client.try_lock().unwrap().packet_handler.complete(client2);

            let logger = logger.clone();
            let client = client.clone();
            let client2 = client.clone();
            let options = options.clone();

            // Create a connection
            debug!(logger, "Connecting"; "address" => %addr);
            let connect_fut = client::connect(Arc::downgrade(&client),
                &mut *client.lock().unwrap(), addr).from_err();

            // Poll the connection for packets
            /*let initserver_poll = initserver_recv
                .and_then(move |cmd| {
                    let cmd = cmd.get_commands().remove(0);
                    let notif = messages::Message::parse(cmd)?;
                    if let messages::Message::InitServer(p) = notif {
                        let con = {
                            let mut client = client2.borrow_mut();
                            client.connection_manager
                                .get_connection(addr).unwrap()
                        };

                        // Create the connection
                        let inner = InnerConnection {
                            connection: Arc::new(Mutex::new(data::Connection::new())),
                            client_data: client2,
                            client_connection: con,
                        };
                        Ok(Connection { inner })
                    } else {
                        Err(Error::ConnectionFailed(
                            String::from("Got no initserver")))
                    }
                });*/

            let initserver_poll = initserver_recv.into_future()
                .map_err(|e| format_err!("Error while waiting for initserver \
                    ({:?})", e).into())
                .and_then(move |(cmd, _)| {
                    let cmd = match cmd {
                        Some(c) => c,
                        None => return Err(Error::ConnectionFailed(
                            String::from("Got no initserver"))),
                    };
                    let cmd = cmd.get_commands().remove(0);
                    let notif = messages::Message::parse(cmd)?;
                    if let messages::Message::InitServer(p) = notif {
                        Ok(p)
                    } else {
                        Err(Error::ConnectionFailed(
                            String::from("Got no initserver")))
                    }
                });

            Box::new(connect_fut
                .and_then(move |con| {
                    // TODO Add possibility to specify offset and level in ConnectOptions
                    // Compute hash cash
                    let mut time_reporter = slog_perf::TimeReporter::new_with_level(
                        "Compute public key hash cash level", logger.clone(),
                        slog::Level::Info);
                    time_reporter.start("Compute public key hash cash level");
                    let (offset, omega) = {
                        let mut c = client.lock().unwrap();
                        let pub_k = c.private_key.to_pub();
                        // TODO Run as blocking future
                        (algs::hash_cash(&pub_k, 8).unwrap(),
                        pub_k.to_ts().unwrap())
                    };
                    time_reporter.finish();
                    info!(logger, "Computed hash cash level";
                        "level" => algs::get_hash_cash_level(&omega, offset),
                        "offset" => offset);

                    // Create clientinit packet
                    let header = Header::new(PacketType::Command);
                    let mut command = commands::Command::new("clientinit");
                    command.push("client_nickname", options.name);
                    command.push("client_version", options.version.get_version_string());
                    command.push("client_platform", options.version.get_platform());
                    command.push("client_input_hardware", "1");
                    command.push("client_output_hardware", "1");
                    command.push("client_default_channel", "");
                    command.push("client_default_channel_password", "");
                    command.push("client_server_password", "");
                    command.push("client_meta_data", "");
                    command.push("client_version_sign", base64::encode(
                        options.version.get_signature()));
                    command.push("client_key_offset", offset.to_string());
                    command.push("client_nickname_phonetic", "");
                    command.push("client_default_token", "");
                    command.push("hwid", "123,456");
                    let p_data = packets::Data::Command(command);
                    let clientinit_packet = Packet::new(header, p_data);

                    let sink = con.as_packet_sink();

                    sink.send(clientinit_packet).map(move |_| con)
                })
                .from_err()
                // Wait until we sent the clientinit packet and afterwards received
                // the initserver packet.
                .and_then(move |con| initserver_poll.map(|r| (con, r)))
                .and_then(move |(con, initserver)| {
                    // Create connection
                    let data = data::Connection::new(Uid("TODO".to_string()),
                        &initserver);
                    let con = InnerConnection {
                        connection: Arc::new(Mutex::new(data)),
                        client_data: client2,
                        client_connection: con,
                    };
                    Ok(Connection { inner: con })
                }))
        })
        .then(move |r| -> Result<_> {
            if let Err(e) = &r {
                debug!(logger2, "Connecting failed, trying next address";
                    "error" => ?e);
            }
            Ok(r.ok())
        })
        .filter_map(|r| r)
        .into_future()
        .map_err(|_| Error::from(format_err!("Failed to connect to server")))
        .and_then(|(r, _)| r.ok_or_else(|| format_err!("Failed to connect to server").into()))
        )
    }

    /// **This is part of the unstable interface.**
    ///
    /// You can use it if you need access to lower level functions, but this
    /// interface may change, even on patch version changes.
    pub fn get_packet_sink(&self) {
    }

    /// **This is part of the unstable interface.**
    ///
    /// You can use it if you need access to lower level functions, but this
    /// interface may change, even on patch version changes.
    pub fn get_udp_packet_sink(&self) {
    }

    /// **This is part of the unstable interface.**
    ///
    /// You can use it if you need access to lower level functions, but this
    /// interface may change, even on patch version changes.
    ///
    /// Adds a `return_code` to the command and returns if the corresponding
    /// answer is received. If an error occurs, the future will return an error.
    pub fn send_command(&self, command: Command) {
        // Store waiting in HashMap<usize (return code), oneshot::Sender>
        // The packet handler then sends a result to the sender if the answer is
        // received.
    }

    pub fn lock(&self) -> ConnectionLock {
        ConnectionLock::new(self.inner.connection.lock().unwrap())
    }

    pub fn to_mut<'a>(&self, con: &'a data::Connection)
        -> data::ConnectionMut<'a> {
        data::ConnectionMut {
            connection: self.inner.clone(),
            inner: &con,
        }
    }

    pub fn disconnect<O: Into<Option<DisconnectOptions>>>(self, options: O)
        -> BoxFuture<()> {
        let options = options.into().unwrap_or_default();

        // TODO Send as message/command
        let header = Header::new(PacketType::Command);
        let mut command = commands::Command::new("clientdisconnect");

        if let Some(reason) = options.reason {
            command.push("reasonid", (reason as u8).to_string());
        }
        if let Some(msg) = options.message {
            command.push("reasonmsg", msg);
        }

        let p_data = packets::Data::Command(command);
        let packet = Packet::new(header, p_data);

        let wait_for_state = client::wait_for_state(&self.inner.client_connection, |state| {
            if let client::ServerConnectionState::Disconnected = state {
                true
            } else {
                false
            }
        });
        Box::new(self.inner.client_connection.as_packet_sink().send(packet)
            .and_then(move |_| wait_for_state)
            .from_err()
            .map(move |_| drop(self)))
    }
}

impl<'a> ConnectionLock<'a> {
    fn new(guard: MutexGuard<'a, data::Connection>) -> Self {
        Self { guard }
    }
}

/// The connection manager which can be shared and cloned.
#[cfg(TODO)]
struct InnerCM {
    handle: Handle,
    logger: Logger,
    connections: HashMap<ConnectionId, structs::NetworkWrapper>,
}

#[cfg(TODO)]
impl InnerCM {
    /// Returns the first free connection id.
    fn find_connection_id(&self) -> ConnectionId {
        for i in 0..self.connections.len() + 1 {
            let id = ConnectionId(i);
            if !self.connections.contains_key(&id) {
                return id;
            }
        }
        unreachable!("Found no free connection id, this should not happen");
    }
}

/// The main type of this crate, which holds all connections.
///
/// It can be created with the [`ConnectionManager::new`] function:
///
/// ```
/// # extern crate tokio;
/// # extern crate tsclientlib;
/// # use std::boxed::Box;
/// # use std::error::Error;
/// #
/// # use tsclientlib::ConnectionManager;
/// # fn main() {
/// #
/// let cm = ConnectionManager::new();
/// # }
/// ```
///
/// [`ConnectionManager::new`]: #method.new
#[cfg(TODO)]
pub struct ConnectionManager {
    inner: Rc<RefCell<InnerCM>>,
    /// The index of the connection which should be polled next.
    ///
    /// This is used to ensure that connections don't starve if one connection
    /// always returns something. It is used in the `Stream` implementation of
    /// `ConnectionManager`.
    poll_index: usize,
    /// The task of the current `Run`, which polls all connections.
    ///
    /// It will be notified when a new connection is added.
    task: Option<Task>,
}

#[cfg(TODO)]
impl ConnectionManager {
    /// Creates a new `ConnectionManager` which is then used to add new
    /// connections.
    ///
    /// Connecting to a server is done by [`ConnectionManager::add_connection`].
    ///
    /// # Example
    ///
    /// ```
    /// # extern crate tokio;
    /// # extern crate tsclientlib;
    /// # use std::boxed::Box;
    /// # use std::error::Error;
    /// #
    /// # use tsclientlib::ConnectionManager;
    /// # fn main() {
    /// #
    /// let cm = ConnectionManager::new();
    /// # }
    /// ```
    ///
    /// [`ConnectionManager::add_connection`]: #method.add_connection
    pub fn new() -> Self {
        // Initialize tsproto if it was not done yet
        static TSPROTO_INIT: Once = ONCE_INIT;
        TSPROTO_INIT.call_once(|| tsproto::init()
            .expect("tsproto failed to initialize"));

        // TODO Create with builder so the logger is optional
        // Don't log anything to console as default setting
        // Option to log to a file
        let logger = {
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator).build().fuse();
            let drain = slog_async::Async::new(drain).build().fuse();

            slog::Logger::root(drain, o!())
        };

        Self {
            inner: Rc::new(RefCell::new(InnerCM {
                handle,
                logger,
                connections: HashMap::new(),
            })),
            poll_index: 0,
            task: None,
        }
    }

    /// Connect to a server.
    pub fn add_connection(&mut self, mut config: ConnectOptions) -> Connect {
        let res: BoxFuture<ConnectionId> = {
            let inner = self.inner.borrow();
            // Try all addresses
            let addr = config.address.resolve(&inner.logger, inner.handle.clone());
            let private_key = match config.private_key.take().map(Ok)
                .unwrap_or_else(|| {
                    // Create new ECDH key
                    crypto::EccKeyPrivP256::create()
                }) {
                Ok(key) => key,
                Err(error) => return Connect::new_from_error(error.into()),
            };

            let logger = inner.logger.clone();
            let handle = inner.handle.clone();
            let inner = Rc::downgrade(&self.inner);

            let logger2 = logger.clone();

            Box::new(addr.and_then(move |addr| -> Box<Future<Item=_, Error=_>> {
                let client = match client::ClientData::new(
                    config.local_address.unwrap_or_else(|| if addr.is_ipv4() {
                        "0.0.0.0:0".parse().unwrap()
                    } else {
                        "[::]:0".parse().unwrap()
                    }),
                    private_key.clone(),
                    handle.clone(),
                    true,
                    tsproto::connectionmanager::SocketConnectionManager::new(),
                    logger.clone(),
                ) {
                    Ok(client) => client,
                    Err(error) => return Box::new(future::err(error.into())),
                };

                // Set the data reference
                {
                    let c2 = client.clone();
                    let mut client = client.borrow_mut();
                    client.connection_manager.set_data_ref(Rc::downgrade(&c2));
                }
                client::default_setup(&client, config.log_packets);

                let logger = logger.clone();
                let inner = inner.clone();
                let client = client.clone();
                let client2 = client.clone();
                let config = config.clone();

                // Create a connection
                debug!(logger, "Connecting"; "addr" => %addr);
                let connect_fut = client::connect(&client, addr);

                // Poll the connection for packets
                let initserver_poll = client::ClientData::get_packets(
                    Rc::downgrade(&client))
                    .filter_map(|(_, p)| {
                        // Filter commands
                        if let Packet { data: packets::Data::Command(cmd), .. } = p {
                            Some(cmd)
                        } else {
                            None
                        }
                    })
                    .into_future().map_err(|(e, _)| e.into())
                    .and_then(move |(cmd, _)| -> BoxFuture<_> {
                        let cmd = if let Some(cmd) = cmd {
                            cmd
                        } else {
                            return Box::new(future::err(Error::ConnectionFailed(
                                String::from("Connection ended"))));
                        };

                        let cmd = cmd.get_commands().remove(0);
                        let notif = tryf!(messages::Message::parse(cmd));
                        if let messages::Message::InitServer(p) = notif {
                            // Create a connection id
                            let inner = inner.upgrade().expect(
                                "Connection manager does not exist anymore");
                            let mut inner = inner.borrow_mut();
                            let id = inner.find_connection_id();

                            let con;
                            {
                                let mut client = client2.borrow_mut();
                                con = client.connection_manager
                                    .get_connection(addr).unwrap();
                            }

                            // Create the connection
                            let con = structs::NetworkWrapper::new(id, client2,
                                Rc::downgrade(&con), &p);

                            // Add the connection
                            inner.connections.insert(id, con);

                            Box::new(future::ok(id))
                        } else {
                            Box::new(future::err(Error::ConnectionFailed(
                                String::from("Got no initserver"))))
                        }
                    });

                Box::new(connect_fut.and_then(move |()| {
                    // TODO Add possibility to specify offset and level in ConnectOptions
                    // Compute hash cash
                    let mut time_reporter = slog_perf::TimeReporter::new_with_level(
                        "Compute public key hash cash level", logger.clone(),
                        slog::Level::Info);
                    time_reporter.start("Compute public key hash cash level");
                    let (offset, omega) = {
                        let mut c = client.borrow_mut();
                        let pub_k = c.private_key.to_pub();
                        (algs::hash_cash(&pub_k, 8).unwrap(),
                        pub_k.to_ts().unwrap())
                    };
                    time_reporter.finish();
                    info!(logger, "Computed hash cash level";
                        "level" => algs::get_hash_cash_level(&omega, offset),
                        "offset" => offset);

                    // Create clientinit packet
                    let header = Header::new(PacketType::Command);
                    let mut command = commands::Command::new("clientinit");
                    command.push("client_nickname", config.name);
                    command.push("client_version", config.version.get_version_string());
                    command.push("client_platform", config.version.get_platform());
                    command.push("client_input_hardware", "1");
                    command.push("client_output_hardware", "1");
                    command.push("client_default_channel", "");
                    command.push("client_default_channel_password", "");
                    command.push("client_server_password", "");
                    command.push("client_meta_data", "");
                    command.push("client_version_sign", base64::encode(
                        config.version.get_signature()));
                    command.push("client_key_offset", offset.to_string());
                    command.push("client_nickname_phonetic", "");
                    command.push("client_default_token", "");
                    command.push("hwid", "123,456");
                    let p_data = packets::Data::Command(command);
                    let clientinit_packet = Packet::new(header, p_data);

                    let sink = Data::get_packets(Rc::downgrade(&client));

                    sink.send((addr, clientinit_packet))
                })
                .from_err()
                // Wait until we sent the clientinit packet and afterwards received
                // the initserver packet.
                .join(initserver_poll)
                .map(|(_, id)| id))
            })
            .then(move |r| -> Result<_> {
                if let Err(e) = &r {
                    debug!(logger2, "Connecting failed, trying next address"; "error" => ?e);
                }
                Ok(r.ok())
            })
            .filter_map(|r: Option<ConnectionId>| r)
            .into_future()
            .map_err(|_| Error::from(format_err!("Failed to connect to server")))
            .and_then(|(r, _)| r.ok_or_else(|| format_err!("Failed to connect to server").into()))
            )
        };
        Connect::new_from_future(self.run().select2(res))
    }

    /// Disconnect from a server.
    ///
    /// # Arguments
    /// - `id`: The connection which should be removed.
    /// - `options`: Either `None` or `DisconnectOptions`.
    ///
    /// # Examples
    ///
    /// Use default options:
    ///
    /// ```rust,no_run
    /// # extern crate tokio_core;
    /// # extern crate tsclientlib;
    /// # use std::boxed::Box;
    /// #
    /// # use tsclientlib::{ConnectionId, ConnectionManager};
    /// # fn main() {
    /// #
    /// let mut core = tokio_core::reactor::Core::new().unwrap();
    /// let mut cm = ConnectionManager::new(core.handle());
    ///
    /// // Add connection...
    ///
    /// # let con_id = ConnectionId(0);
    /// let disconnect_future = cm.remove_connection(con_id, None);
    /// core.run(disconnect_future).unwrap();
    /// # }
    /// ```
    ///
    /// Specify a reason and a quit message:
    ///
    /// ```rust,no_run
    /// # extern crate tokio_core;
    /// # extern crate tsclientlib;
    /// # use std::boxed::Box;
    /// #
    /// # use tsclientlib::{ConnectionId, ConnectionManager, DisconnectOptions,
    /// # Reason};
    /// # fn main() {
    /// #
    /// # let mut core = tokio_core::reactor::Core::new().unwrap();
    /// # let mut cm = ConnectionManager::new(core.handle());
    /// # let con_id = ConnectionId(0);
    /// cm.remove_connection(con_id, DisconnectOptions::new()
    ///     .reason(Reason::Clientdisconnect)
    ///     .message("Away for a while"));
    /// # }
    /// ```
    pub fn remove_connection<O: Into<Option<DisconnectOptions>>>(&mut self,
        id: ConnectionId, options: O) -> Disconnect {
        let client_con;
        let client_data;
        {
            let inner_b = self.inner.borrow();
            if let Some(con) = inner_b.connections.get(&id) {
                client_con = con.client_connection.clone();
                client_data = con.client_data.clone();
            } else {
                return Disconnect::new_from_ok();
            }
        }

        let client_con = if let Some(c) = client_con.upgrade() {
            c
        } else {
            // Already disconnected
            return Disconnect::new_from_ok();
        };

        let header = Header::new(PacketType::Command);
        let mut command = commands::Command::new("clientdisconnect");

        let options = options.into().unwrap_or_default();
        if let Some(reason) = options.reason {
            command.push("reasonid", (reason as u8).to_string());
        }
        if let Some(msg) = options.message {
            command.push("reasonmsg", msg);
        }

        let p_data = packets::Data::Command(command);
        let packet = Packet::new(header, p_data);

        let addr;
        {
            let mut con = client_con.borrow_mut();
            con.resender.handle_event(ResenderEvent::Disconnecting);
            addr = con.address;
        }

        let sink = Data::get_packets(Rc::downgrade(&client_data));
        let wait_for_state = client::wait_for_state(&client_data, addr, |state| {
            if let client::ServerConnectionState::Disconnected = *state {
                true
            } else {
                false
            }
        });
        let fut: BoxFuture<_> = Box::new(sink.send((addr, packet))
            .and_then(move |_| wait_for_state)
            .from_err());
        Disconnect::new_from_future(self.run().select(fut))
    }

    #[inline]
    pub fn get_connection(&self, id: ConnectionId) -> Option<Connection> {
        if self.inner.borrow().connections.contains_key(&id) {
            Some(Connection { cm: self, id })
        } else {
            None
        }
    }

    #[inline]
    pub fn get_mut_connection(&mut self, id: ConnectionId) -> Option<ConnectionMut> {
        if self.inner.borrow().connections.contains_key(&id) {
            Some(ConnectionMut { cm: self, id })
        } else {
            None
        }
    }

    #[inline]
    /// Creates a future to handle all packets.
    pub fn run(&mut self) -> Run {
        Run { cm: self }
    }
}

// Private methods
#[cfg(TODO)]
impl ConnectionManager {
    fn get_file(&self, _con: ConnectionId, _chan: ChannelId, _path: &str, _file: &str) -> Ref<structs::File> {
        unimplemented!("File transfer is not yet implemented")
    }

    fn get_chat_entry(&self, _con: ConnectionId, _sender: ClientId) -> Ref<structs::ChatEntry> {
        unimplemented!("Chatting is not yet implemented")
    }

    /// Poll like a stream to get the next message.
    fn poll_stream(&mut self) -> futures::Poll<Option<(ConnectionId, Message)>,
        Error> {
        if !self.task.as_ref().map(|t| t.will_notify_current()).unwrap_or(false) {
            self.task = Some(task::current());
        }

        // Poll all connections
        let inner = &mut *self.inner.borrow_mut();
        let keys: Vec<_> = inner.connections.keys().cloned().collect();
        if keys.is_empty() {
            // Wait until our task gets notified
            return Ok(futures::Async::NotReady);
        }

        if self.poll_index >= keys.len() {
            self.poll_index = 0;
        }

        let mut remove_connection = false;
        let mut result = Ok(futures::Async::NotReady);
        for con_id in 0..keys.len() {
            let i = (self.poll_index + con_id) % keys.len();
            let con = inner.connections.get_mut(&keys[i]).unwrap();
            match con.poll() {
                Ok(futures::Async::Ready(None)) =>
                    warn!(inner.logger, "Got None from a connection";
                        "connection" => %keys[i].0),
                Ok(futures::Async::Ready(Some((_, res)))) => {
                    // Check if the connection is still alive
                    if con.client_connection.upgrade().is_none() {
                        remove_connection = true;
                    }
                    self.poll_index = i + 1;
                    result = Ok(futures::Async::Ready(Some((keys[i], res))));
                    break;
                }
                Ok(futures::Async::NotReady) => {}
                Err(error) =>
                    warn!(inner.logger, "Got an error from a connection";
                        "error" => ?error, "connection" => %keys[i].0),
            }
        }
        if remove_connection {
            // Remove the connection
            inner.connections.remove(&keys[self.poll_index - 1]);
        }
        result
    }

    #[inline]
    // Poll like a future created by (ConnectionManager as Stream).for_each().
    fn poll_future(&mut self) -> futures::Poll<(), Error> {
        loop {
            match self.poll_stream()? {
                futures::Async::Ready(Some(_)) => {}
                futures::Async::Ready(None) =>
                    return Ok(futures::Async::Ready(())),
                futures::Async::NotReady => return Ok(futures::Async::NotReady),
            }
        }
    }
}

#[cfg(TODO)]
impl fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ConnectionManager(...)")
    }
}

/// A future which runs the `ConnectionManager` indefinitely.
#[cfg(TODO)]
#[derive(Debug)]
pub struct Run<'a> {
    cm: &'a mut ConnectionManager,
}

#[cfg(TODO)]
impl<'a> Future for Run<'a> {
    type Item = ();
    type Error = Error;

    #[inline]
    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        self.cm.poll_future()
    }
}

#[cfg(TODO)]
pub struct Connect<'a> {
    /// Contains an error if the `add_connection` functions should return an
    /// error.
    inner: Either<Option<Error>,
        futures::future::Select2<Run<'a>, BoxFuture<ConnectionId>>>,
}

#[cfg(TODO)]
impl<'a> Connect<'a> {
    fn new_from_error(error: Error) -> Self {
        Self { inner: Either::A(Some(error)) }
    }

    fn new_from_future(future: futures::future::Select2<Run<'a>,
        BoxFuture<ConnectionId>>) -> Self {
        Self { inner: Either::B(future) }
    }
}

#[cfg(TODO)]
impl<'a> Future for Connect<'a> {
    type Item = ConnectionId;
    type Error = Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        match self.inner {
            // Take the error, this will panic if called twice
            Either::A(ref mut error) => Err(error.take().unwrap()),
            Either::B(ref mut inner) => match inner.poll() {
                Ok(futures::Async::Ready(Either::A(((), _)))) =>
                    Err(format_err!("Could not connect").into()),
                Ok(futures::Async::Ready(Either::B((id, _)))) =>
                    Ok(futures::Async::Ready(id)),
                Ok(futures::Async::NotReady) => Ok(futures::Async::NotReady),
                Err(Either::A((error, _))) |
                Err(Either::B((error, _))) => Err(error),
            }
        }
    }
}

#[cfg(TODO)]
pub struct Disconnect<'a> {
    inner: Option<futures::future::Select<Run<'a>, BoxFuture<()>>>,
}

#[cfg(TODO)]
impl<'a> Disconnect<'a> {
    fn new_from_ok() -> Self {
        Self { inner: None }
    }

    fn new_from_future(future: futures::future::Select<Run<'a>, BoxFuture<()>>)
        -> Self {
        Self { inner: Some(future) }
    }
}

#[cfg(TODO)]
impl<'a> Future for Disconnect<'a> {
    type Item = ();
    type Error = Error;

    #[inline]
    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        match self.inner {
            None => Ok(futures::Async::Ready(())),
            Some(ref mut f) => f.poll().map(|r| match r {
                futures::Async::Ready(_) => futures::Async::Ready(()),
                futures::Async::NotReady => futures::Async::NotReady,
            }).map_err(|(e, _)| e),
        }
    }
}

#[cfg(TODO)]
impl<'a> Connection<'a> {
    #[inline]
    pub fn get_server(&self) -> Server {
        Server {
            cm: self.cm,
            connection_id: self.id,
        }
    }
}

#[cfg(TODO)]
impl<'a> ConnectionMut<'a> {
    #[inline]
    pub fn get_server(&self) -> Server {
        Server {
            cm: self.cm,
            connection_id: self.id,
        }
    }

    #[inline]
    pub fn get_mut_server(&mut self) -> ServerMut {
        ServerMut {
            cm: self.cm,
            connection_id: self.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerAddress {
    SocketAddr(SocketAddr),
    Other(String),
}

impl From<SocketAddr> for ServerAddress {
    fn from(addr: SocketAddr) -> Self {
        ServerAddress::SocketAddr(addr)
    }
}

impl From<String> for ServerAddress {
    fn from(addr: String) -> Self {
        ServerAddress::Other(addr)
    }
}

impl<'a> From<&'a str> for ServerAddress {
    fn from(addr: &'a str) -> Self {
        ServerAddress::Other(addr.to_string())
    }
}

impl ServerAddress {
    pub fn resolve(&self, logger: &Logger) -> Box<Stream<Item=SocketAddr, Error=Error> + Send> {
        match self {
            ServerAddress::SocketAddr(a) => Box::new(stream::once(Ok(*a))),
            ServerAddress::Other(s) => Box::new(resolver::resolve(logger, s)),
        }
    }
}

impl fmt::Display for ServerAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ServerAddress::SocketAddr(a) => fmt::Display::fmt(a, f),
            ServerAddress::Other(a) => fmt::Display::fmt(a, f),
        }
    }
}

/// The configuration used to create a new connection.
///
/// This is a builder for a connection.
///
/// # Example
///
/// ```rust,no_run
/// # extern crate tokio_core;
/// # extern crate tsclientlib;
/// #
/// # use tsclientlib::{ConnectionManager, ConnectOptions};
/// # fn main() {
/// #
/// let mut core = tokio_core::reactor::Core::new().unwrap();
///
/// let con_config = ConnectOptions::new("localhost");
///
/// let mut cm = ConnectionManager::new(core.handle());
/// let con = core.run(cm.add_connection(con_config)).unwrap();
/// # }
/// ```
pub struct ConnectOptions {
    address: ServerAddress,
    local_address: Option<SocketAddr>,
    private_key: Option<crypto::EccKeyPrivP256>,
    name: String,
    version: Version,
    logger: Option<Logger>,
    log_packets: bool,
    handle_packets: Option<PHBox>,
}

impl ConnectOptions {
    /// Start creating the configuration of a new connection.
    ///
    /// # Arguments
    /// The address of the server has to be supplied. The address can be a
    /// [`SocketAddr`], a [`String`] or directly a [`ServerAddress`]. A string
    /// will automatically be resolved from all formats supported by TeamSpeak.
    /// For details, see [`resolver::resolve`].
    ///
    /// [`SocketAddr`]: ../../std/net/enum.SocketAddr.html
    /// [`String`]: ../../std/string/struct.String.html
    /// [`ServerAddress`]: enum.ServerAddress.html
    /// [`resolver::resolve`]: resolver/method.resolve.html
    #[inline]
    pub fn new<A: Into<ServerAddress>>(address: A) -> Self {
        Self {
            address: address.into(),
            local_address: None,
            private_key: None,
            name: String::from("TeamSpeakUser"),
            version: Version::Linux_3_2_1,
            logger: None,
            log_packets: false,
            handle_packets: None,
        }
    }

    /// The address for the socket of our client
    ///
    /// # Default
    /// The default is `0.0.0:0` when connecting to an IPv4 address and `[::]:0`
    /// when connecting to an IPv6 address.
    #[inline]
    pub fn local_address(mut self, local_address: SocketAddr) -> Self {
        self.local_address = Some(local_address);
        self
    }

    /// Set the private key of the user.
    ///
    /// # Default
    /// A new identity is generated when connecting.
    #[inline]
    pub fn private_key(mut self, private_key: crypto::EccKeyPrivP256)
        -> Self {
        self.private_key = Some(private_key);
        self
    }

    /// Takes the private key as encoded by TeamSpeak (libtomcrypt export and
    /// base64 encoded).
    ///
    /// # Default
    /// A new identity is generated when connecting.
    ///
    /// # Error
    /// An error is returned if either the string is not encoded in valid base64
    /// or libtomcrypt cannot import the key.
    #[inline]
    pub fn private_key_ts(mut self, private_key: &str) -> Result<Self> {
        self.private_key = Some(crypto::EccKeyPrivP256::from_ts(private_key)?);
        Ok(self)
    }

    /// The name of the user.
    ///
    /// # Default
    /// `TeamSpeakUser`
    #[inline]
    pub fn name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// The displayed version of the client.
    ///
    /// # Default
    /// `3.2.1 on Linux`
    #[inline]
    pub fn version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }

    /// If the content of all packets in high-level and byte-array form should
    /// be written to the logger.
    ///
    /// # Default
    /// `false`
    #[inline]
    pub fn log_packets(mut self, log_packets: bool) -> Self {
        self.log_packets = log_packets;
        self
    }

    /// Set a custom logger for the connection.
    ///
    /// # Default
    /// A new logger is created.
    #[inline]
    pub fn logger(mut self, logger: Logger) -> Self {
        self.logger = Some(logger);
        self
    }

    /// Handle incomming command and audio packets in a custom way,
    /// additionally to the default handling.
    ///
    /// The given function will be called with a stream of command packets and a
    /// second stream of audio packets.
    ///
    /// # Default
    /// Packets are handled in the default way and then dropped.
    #[inline]
    pub fn handle_packets(mut self,
        handle_packets: PHBox) -> Self {
        self.handle_packets = Some(handle_packets);
        self
    }
}

impl fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Error if attributes are added
        let ConnectOptions {
            address, local_address, private_key, name, version, logger,
            log_packets, handle_packets: _,
        } = self;
        write!(f, "ConnectOptions {{ \
            address: {:?}, \
            local_address: {:?}, \
            private_key: {:?}, \
            name: {}, \
            version: {}, \
            logger: {:?}, \
            log_packets: {}, \
            }}", address, local_address, private_key, name, version, logger,
            log_packets)?;
        Ok(())
    }
}

impl Clone for ConnectOptions {
    fn clone(&self) -> Self {
        ConnectOptions {
            address: self.address.clone(),
            local_address: self.local_address.clone(),
            private_key: self.private_key.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            logger: self.logger.clone(),
            log_packets: self.log_packets.clone(),
            handle_packets: self.handle_packets.as_ref()
                .map(|h| h.as_ref().clone()),
        }
    }
}

pub struct DisconnectOptions {
    reason: Option<Reason>,
    message: Option<String>,
}

impl Default for DisconnectOptions {
    #[inline]
    fn default() -> Self {
        Self {
            reason: None,
            message: None,
        }
    }
}

impl DisconnectOptions {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the reason for leaving.
    ///
    /// # Default
    ///
    /// None
    #[inline]
    pub fn reason(mut self, reason: Reason) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Set the leave message.
    ///
    /// You also have to set the reason, otherwise the message will not be
    /// displayed.
    ///
    /// # Default
    ///
    /// None
    #[inline]
    pub fn message<S: Into<String>>(mut self, message: S) -> Self {
        self.message = Some(message.into());
        self
    }
}
