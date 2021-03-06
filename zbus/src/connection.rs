use std::cell::{Cell, RefCell};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use nix::poll::PollFlags;
use once_cell::unsync::OnceCell;

use crate::handshake::{Authenticated, ClientHandshake, ServerHandshake};
use crate::raw::Connection as RawConnection;
use crate::utils::wait_on;
use crate::{fdo, Error, Guid, Message, MessageType, Result};

type MessageHandlerFn = Box<dyn FnMut(Message) -> Option<Message>>;

const DEFAULT_MAX_QUEUED: usize = 32;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
struct ConnectionInner {
    server_guid: Guid,
    cap_unix_fd: bool,
    unique_name: OnceCell<String>,

    raw_conn: RefCell<RawConnection<UnixStream>>,
    // Serial number for next outgoing message
    serial: Cell<u32>,

    // Queue of incoming messages
    incoming_queue: RefCell<Vec<Message>>,

    // Max number of messages to queue
    max_queued: Cell<usize>,

    #[derivative(Debug = "ignore")]
    default_msg_handler: RefCell<Option<MessageHandlerFn>>,
}

/// A D-Bus connection.
///
/// A connection to a D-Bus bus, or a direct peer.
///
/// Once created, the connection is authenticated and negotiated and messages can be sent or
/// received, such as [method calls] or [signals].
///
/// For higher-level message handling (typed functions, introspection, documentation reasons etc),
/// it is recommended to wrap the low-level D-Bus messages into Rust functions with the
/// [`dbus_proxy`] and [`dbus_interface`] macros instead of doing it directly on a `Connection`.
///
/// For lower-level handling of the connection (such as nonblocking socket handling), see the
/// documentation of the [`new_authenticated_unix`] constructor.
///
/// Typically, a connection is made to the session bus with [`new_session`], or to the system bus
/// with [`new_system`]. Then the connection is shared with the [`Proxy`] and [`ObjectServer`]
/// instances.
///
/// `Connection` implements [`Clone`] and cloning it is a very cheap operation, as the underlying
/// data is not cloned. This makes it very convenient to share the connection between different
/// parts of your code. Please note however, that sharing or sending of a connection instance
/// across threads is not supported. If you've a valid use cas for that, please [file an issue]
/// about it and we'll consider adding this feature.
///
/// Since there are times when important messages arrive between a method call message is sent and
/// its reply is received, `Connection` keeps an internal queue of incoming messages so that these
/// messages are not lost and subsequent calls to [`receive_message`] will retreive messages from
/// this queue first. The size of this queue is configurable through the [`set_max_queued`] method.
/// The default size is 32. All messages that are received after the queue is full, are dropped.
///
/// [method calls]: struct.Connection.html#method.call_method
/// [signals]: struct.Connection.html#method.emit_signal
/// [`new_system`]: struct.Connection.html#method.new_system
/// [`new_session`]: struct.Connection.html#method.new_session
/// [`new_authenticated_unix`]: struct.Connection.html#method.new_authenticated_unix
/// [`Proxy`]: struct.Proxy.html
/// [`ObjectServer`]: struct.ObjectServer.html
/// [`dbus_proxy`]: attr.dbus_proxy.html
/// [`dbus_interface`]: attr.dbus_interface.html
/// [`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html
/// [file an issue]: https://gitlab.freedesktop.org/zeenix/zbus/-/issues/new
/// [`receive_message`]: struct.Connection.html#method.receive_message
/// [`set_max_queued`]: struct.Connection.html#method.set_max_queued
#[derive(Debug, Clone)]
pub struct Connection(Rc<ConnectionInner>);

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.0.raw_conn.borrow().socket().as_raw_fd()
    }
}

impl Connection {
    /// Create and open a D-Bus connection from a `UnixStream`.
    ///
    /// The connection may either be set up for a *bus* connection, or not (for peer-to-peer
    /// communications).
    ///
    /// Upon successful return, the connection is fully established and negotiated: D-Bus messages
    /// can be sent and received.
    pub fn new_unix_client(stream: UnixStream, bus_connection: bool) -> Result<Self> {
        // SASL Handshake
        let auth = ClientHandshake::new(stream).blocking_finish()?;

        if bus_connection {
            Connection::new_authenticated_unix_bus(auth)
        } else {
            Ok(Connection::new_authenticated_unix(auth))
        }
    }

    /// Create a `Connection` to the session/user message bus.
    pub fn new_session() -> Result<Self> {
        ClientHandshake::new_session()?
            .blocking_finish()
            .and_then(Self::new_authenticated_unix_bus)
    }

    /// Create a `Connection` to the system-wide message bus.
    pub fn new_system() -> Result<Self> {
        ClientHandshake::new_system()?
            .blocking_finish()
            .and_then(Self::new_authenticated_unix_bus)
    }

    /// Create a `Connection` for the given [D-Bus address].
    ///
    /// [D-Bus address]: https://dbus.freedesktop.org/doc/dbus-specification.html#addresses
    pub fn new_for_address(address: &str, bus_connection: bool) -> Result<Self> {
        let auth = ClientHandshake::new_for_address(address)?.blocking_finish()?;

        if bus_connection {
            Connection::new_authenticated_unix_bus(auth)
        } else {
            Ok(Connection::new_authenticated_unix(auth))
        }
    }

    /// Create a server `Connection` for the given `UnixStream` and the server `guid`.
    ///
    /// The connection will wait for incoming client authentication handshake & negotiation messages,
    /// for peer-to-peer communications.
    ///
    /// Upon successful return, the connection is fully established and negotiated: D-Bus messages
    /// can be sent and received.
    pub fn new_unix_server(stream: UnixStream, guid: &Guid) -> Result<Self> {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

        let creds = getsockopt(stream.as_raw_fd(), PeerCredentials)
            .map_err(|e| Error::Handshake(format!("Failed to get peer credentials: {}", e)))?;

        let handshake = ServerHandshake::new(stream, guid.clone(), creds.uid());
        handshake
            .blocking_finish()
            .map(Connection::new_authenticated_unix)
    }

    /// Max number of messages to queue.
    pub fn max_queued(&self) -> usize {
        self.0.max_queued.get()
    }

    /// Set the max number of messages to queue.
    ///
    /// Since typically you'd want to set this at instantiation time, this method takes ownership
    /// of `self` and returns an owned `Connection` instance so you can use the builder pattern to
    /// set the value.
    ///
    /// # Example
    ///
    /// ```
    ///# use std::error::Error;
    ///#
    /// let conn = zbus::Connection::new_session()?.set_max_queued(30);
    /// assert_eq!(conn.max_queued(), 30);
    ///
    /// // Do something usefull with `conn`..
    ///# Ok::<_, Box<dyn Error + Send + Sync>>(())
    /// ```
    pub fn set_max_queued(self, max: usize) -> Self {
        self.0.max_queued.replace(max);

        self
    }

    /// The server's GUID.
    pub fn server_guid(&self) -> &str {
        self.0.server_guid.as_str()
    }

    /// The unique name as assigned by the message bus or `None` if not a message bus connection.
    pub fn unique_name(&self) -> Option<&str> {
        self.0.unique_name.get().map(|s| s.as_str())
    }

    /// Fetch the next message from the connection.
    ///
    /// Read from the connection until a message is received or an error is reached. Return the
    /// message on success. If the connection is in non-blocking mode, this will return a
    /// `WouldBlock` error instead of blocking. If there are pending messages in the queue, the
    /// first one from the queue is returned instead of attempting to read the connection.
    ///
    /// If a default message handler has been registered on this connection through
    /// [`set_default_message_handler`], it will first get to decide the fate of the received
    /// message.
    ///
    /// [`set_default_message_handler`]: struct.Connection.html#method.set_default_message_handler
    pub fn receive_message(&self) -> Result<Message> {
        let mut queue = self.0.incoming_queue.borrow_mut();
        if let Some(msg) = queue.pop() {
            return Ok(msg);
        }

        loop {
            let incoming = self.0.raw_conn.borrow_mut().try_receive_message()?;

            if let Some(ref mut handler) = &mut *self.0.default_msg_handler.borrow_mut() {
                // Let's see if the default handler wants the message first
                match handler(incoming) {
                    // Message was returned to us so we can return that
                    Some(m) => return Ok(m),
                    None => continue,
                }
            }

            return Ok(incoming);
        }
    }

    /// Send `msg` to the peer.
    ///
    /// The connection sets a unique serial number on the message before sending it off.
    ///
    /// On successfully sending off `msg`, the assigned serial number is returned.
    ///
    /// **Note:** if this connection is in non-blocking mode, the message may not actually
    /// have been sent when this method returns, and you need to call the [`flush`] method
    /// so that pending messages are written to the socket.
    ///
    /// [`flush`]: struct.Connection.html#method.flush
    pub fn send_message(&self, mut msg: Message) -> Result<u32> {
        if !msg.fds().is_empty() && !self.0.cap_unix_fd {
            return Err(Error::Unsupported);
        }

        let serial = self.next_serial();
        msg.modify_primary_header(|primary| {
            primary.set_serial_num(serial);

            Ok(())
        })?;

        let mut conn = self.0.raw_conn.borrow_mut();
        conn.enqueue_message(msg);
        // Swallow a potential WouldBLock error, but propagate the others
        if let Err(e) = conn.try_flush() {
            if e.kind() != std::io::ErrorKind::WouldBlock {
                return Err(e.into());
            }
        }

        Ok(serial)
    }

    /// Flush pending outgoing messages to the server
    ///
    /// This method is only useful if the connection is in non-blocking mode. It will
    /// write as many pending outgoing messages as possible to the socket.
    ///
    /// It will return `Ok(())` if all messages could be sent, and error otherwise. A
    /// `WouldBlock` error means that the internal buffer of the connection transport is
    /// full, and you need to wait for write-readiness before calling this method again.
    ///
    /// If the connection is in blocking mode, this will return `Ok(())` and do nothing.
    pub fn flush(&self) -> Result<()> {
        self.0.raw_conn.borrow_mut().try_flush()?;
        Ok(())
    }

    /// Send a method call.
    ///
    /// Create a method-call message, send it over the connection, then wait for the reply. Incoming
    /// messages are received through [`receive_message`] (and by the default message handler)
    /// until the matching method reply (error or return) is received.
    ///
    /// On succesful reply, an `Ok(Message)` is returned. On error, an `Err` is returned. D-Bus
    /// error replies are returned as [`MethodError`].
    ///
    /// *Note:* This method will block until the response is received even if the connection is
    /// in non-blocking mode. If you don't want to block like this, use [`send_message`].
    ///
    /// [`receive_message`]: struct.Connection.html#method.receive_message
    /// [`MethodError`]: enum.Error.html#variant.MethodError
    /// [`sent_message`]: struct.Connection.html#method.send_message
    pub fn call_method<B>(
        &self,
        destination: Option<&str>,
        path: &str,
        iface: Option<&str>,
        method_name: &str,
        body: &B,
    ) -> Result<Message>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method(
            self.unique_name(),
            destination,
            path,
            iface,
            method_name,
            body,
        )?;

        let serial = self.send_message(m)?;
        // loop & sleep until the message is completely sent
        loop {
            match self.flush() {
                Ok(()) => break,
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_on(self.as_raw_fd(), PollFlags::POLLOUT)?;
                }
                Err(e) => return Err(e),
            }
        }
        let mut tmp_queue = vec![];

        loop {
            let m = loop {
                match self.receive_message() {
                    Ok(m) => break m,
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        wait_on(self.as_raw_fd(), PollFlags::POLLIN)?;
                    }
                    Err(e) => return Err(e),
                }
            };
            let h = m.header()?;

            if h.reply_serial()? != Some(serial) {
                let queue = self.0.incoming_queue.borrow();
                if queue.len() + tmp_queue.len() < self.0.max_queued.get() {
                    // We first push to a temporary queue as otherwise it'll create an infinite loop
                    // since subsequent `receive_message` call will pick up the message from the main
                    // queue.
                    tmp_queue.push(m);
                }

                continue;
            } else {
                self.0.incoming_queue.borrow_mut().append(&mut tmp_queue);
            }

            match h.message_type()? {
                MessageType::Error => return Err(m.into()),
                MessageType::MethodReturn => return Ok(m),
                _ => (),
            }
        }
    }

    /// Emit a signal.
    ///
    /// Create a signal message, and send it over the connection.
    pub fn emit_signal<B>(
        &self,
        destination: Option<&str>,
        path: &str,
        iface: &str,
        signal_name: &str,
        body: &B,
    ) -> Result<()>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::signal(
            self.unique_name(),
            destination,
            path,
            iface,
            signal_name,
            body,
        )?;

        self.send_message(m)?;

        Ok(())
    }

    /// Reply to a message.
    ///
    /// Given an existing message (likely a method call), send a reply back to the caller with the
    /// given `body`.
    ///
    /// Returns the message serial number.
    pub fn reply<B>(&self, call: &Message, body: &B) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method_reply(self.unique_name(), call, body)?;
        self.send_message(m)
    }

    /// Reply an error to a message.
    ///
    /// Given an existing message (likely a method call), send an error reply back to the caller
    /// with the given `error_name` and `body`.
    ///
    /// Returns the message serial number.
    pub fn reply_error<B>(&self, call: &Message, error_name: &str, body: &B) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method_error(self.unique_name(), call, error_name, body)?;
        self.send_message(m)
    }

    /// Set a default handler for incoming messages on this connection.
    ///
    /// This is the handler that will be called on all messages received during [`receive_message`]
    /// call. If the handler callback returns a message (which could be a different message than it
    /// was given), `receive_message` will return it to its caller.
    ///
    /// [`receive_message`]: struct.Connection.html#method.receive_message
    pub fn set_default_message_handler(&mut self, handler: MessageHandlerFn) {
        self.0.default_msg_handler.borrow_mut().replace(handler);
    }

    /// Reset the default message handler.
    ///
    /// Remove the previously set message handler from `set_default_message_handler`.
    pub fn reset_default_message_handler(&mut self) {
        self.0.default_msg_handler.borrow_mut().take();
    }

    /// Create a `Connection` from an already authenticated unix socket
    ///
    /// This method can be used in conjunction with [`ClientHandshake`] or [`ServerHandshake`] to handle
    /// the initial handshake of the D-Bus connection asynchronously. The [`Authenticated`] argument required
    /// by this method is the result provided by these handshake utilities.
    ///
    /// If the aim is to initialize a client *bus* connection, you need to send the [client hello] and assign
    /// the resulting unique name using [`set_unique_name`] before doing anything else.
    ///
    /// [`ClientHandshake`]: ./handshake/struct.ClientHandshake.html
    /// [`ServerHandshake`]: ./handshake/struct.ServerHandshake.html
    /// [`Authenticated`]: ./handshake/struct.Authenticated.html
    /// [client hello]: ./fdo/struct.DBusProxy.html#method.hello
    /// [`set_unique_name`]: struct.Connection.html#method.set_unique_name
    pub fn new_authenticated_unix(auth: Authenticated<UnixStream>) -> Self {
        Self(Rc::new(ConnectionInner {
            raw_conn: RefCell::new(auth.conn),
            server_guid: auth.server_guid,
            cap_unix_fd: auth.cap_unix_fd,
            serial: Cell::new(1),
            unique_name: OnceCell::new(),
            incoming_queue: RefCell::new(vec![]),
            max_queued: Cell::new(DEFAULT_MAX_QUEUED),
            default_msg_handler: RefCell::new(None),
        }))
    }

    /// Sets the unique name for this connection
    ///
    /// This method should only be used when initializing a client *bus* connection with
    /// [`new_authenticated_unix`]. Setting the unique name to anything other than the return value of the bus
    /// hello is a protocol violation.
    ///
    /// Returns and error if the name has already been set.
    ///
    /// [`new_authenticated_unix`]: struct.Connection.html#method.new_authenticated_unix
    pub fn set_unique_name(&self, name: String) -> std::result::Result<(), String> {
        self.0.unique_name.set(name)
    }

    fn new_authenticated_unix_bus(auth: Authenticated<UnixStream>) -> Result<Self> {
        let connection = Connection::new_authenticated_unix(auth);

        // Now that the server has approved us, we must send the bus Hello, as per specs
        let name = fdo::DBusProxy::new(&connection)?
            .hello()
            .map_err(|e| Error::Handshake(format!("Hello failed: {}", e)))?;
        connection
            .0
            .unique_name
            .set(name)
            // programmer (probably our) error if this fails.
            .expect("Attempted to set unique_name twice");

        Ok(connection)
    }

    fn next_serial(&self) -> u32 {
        let next = self.0.serial.get() + 1;

        self.0.serial.replace(next)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;

    use crate::{Connection, Guid};

    #[test]
    fn unix_p2p() {
        let guid = Guid::generate();

        let (p0, p1) = UnixStream::pair().unwrap();

        let server_thread = thread::spawn(move || {
            let c = Connection::new_unix_server(p0, &guid).unwrap();
            let reply = c
                .call_method(None, "/", Some("org.zbus.p2p"), "Test", &())
                .unwrap();
            assert_eq!(reply.to_string(), "Method return");
            let val: String = reply.body().unwrap();
            val
        });

        let c = Connection::new_unix_client(p1, false).unwrap();
        let m = c.receive_message().unwrap();
        assert_eq!(m.to_string(), "Method call Test");
        c.reply(&m, &("yay")).unwrap();

        let val = server_thread.join().expect("failed to join server thread");
        assert_eq!(val, "yay");
    }

    #[test]
    fn serial_monotonically_increases() {
        let c = Connection::new_session().unwrap();
        let serial = c.next_serial() + 1;

        for next in serial..serial + 10 {
            assert_eq!(next, c.next_serial());
        }
    }
}
