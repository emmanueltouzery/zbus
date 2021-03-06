#![doc(
    html_logo_url = "https://storage.googleapis.com/fdo-gitlab-uploads/project/avatar/3213/zbus-logomark.png"
)]

//! This crate provides the main API you will use to interact with D-Bus from Rust. It takes care of
//! the establishment of a connection, the creation, sending and receiving of different kind of
//! D-Bus messages (method calls, signals etc) for you.
//!
//! zbus crate is currently Linux-specific[^otheros].
//!
//! ### Getting Started
//!
//! The best way to get started with zbus is the [book], where we start with basic D-Bus concepts
//! and explain with code samples, how zbus makes D-Bus easy.
//!
//! ### Example code
//!
//! #### Client
//!
//! This code display a notification on your Freedesktop.org-compatible OS:
//!
//! ```rust,no_run
//! use std::collections::HashMap;
//! use std::error::Error;
//!
//! use zbus::dbus_proxy;
//! use zvariant::Value;
//!
//! #[dbus_proxy]
//! trait Notifications {
//!     fn notify(
//!         &self,
//!         app_name: &str,
//!         replaces_id: u32,
//!         app_icon: &str,
//!         summary: &str,
//!         body: &str,
//!         actions: &[&str],
//!         hints: HashMap<&str, &Value>,
//!         expire_timeout: i32,
//!     ) -> zbus::Result<u32>;
//! }
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let connection = zbus::Connection::new_session()?;
//!
//!     let proxy = NotificationsProxy::new(&connection)?;
//!     let reply = proxy.notify(
//!         "my-app",
//!         0,
//!         "dialog-information",
//!         "A summary",
//!         "Some body",
//!         &[],
//!         HashMap::new(),
//!         5000,
//!     )?;
//!     dbg!(reply);
//!
//!     Ok(())
//! }
//! ```
//!
//! #### Server
//!
//! A simple service that politely greets whoever calls its `SayHello` method:
//!
//! ```rust,no_run
//! use std::error::Error;
//! use std::convert::TryInto;
//! use zbus::{dbus_interface, fdo};
//!
//! struct Greeter {
//!     count: u64
//! };
//!
//! #[dbus_interface(name = "org.zbus.MyGreeter1")]
//! impl Greeter {
//!     fn say_hello(&mut self, name: &str) -> String {
//!         self.count += 1;
//!         format!("Hello {}! I have been called: {}", name, self.count)
//!     }
//! }
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let connection = zbus::Connection::new_session()?;
//!     fdo::DBusProxy::new(&connection)?.request_name(
//!         "org.zbus.MyGreeter",
//!         fdo::RequestNameFlags::ReplaceExisting.into(),
//!     )?;
//!
//!     let mut object_server = zbus::ObjectServer::new(&connection);
//!     let mut greeter = Greeter { count: 0 };
//!     object_server.at(&"/org/zbus/MyGreeter".try_into()?, greeter)?;
//!     loop {
//!         if let Err(err) = object_server.try_handle_next() {
//!             eprintln!("{}", err);
//!         }
//!     }
//! }
//! ```
//!
//! You can use the following command to test it:
//!
//! ```bash
//! $ busctl --user call \
//!     org.zbus.MyGreeter \
//!     /org/zbus/MyGreeter \
//!     org.zbus.MyGreeter1 \
//!     SayHello s "Maria"
//! Hello Maria!
//! $
//! ```
//!
//! [book]: https://zeenix.pages.freedesktop.org/zbus/
//!
//! [^otheros]: Support for other OS exist, but it is not supported to the same extent. D-Bus
//!   clients in javascript (running from any browser) do exist though. And zbus may also be
//!   working from the browser sometime in the future too, thanks to Rust 🦀 and WebAssembly 🕸.
//!

#[cfg(doctest)]
mod doctests {
    doc_comment::doctest!("../../README.md");
    // Book markdown checks
    doc_comment::doctest!("../../book/src/client.md");
    doc_comment::doctest!("../../book/src/concepts.md");
    doc_comment::doctest!("../../book/src/connection.md");
    doc_comment::doctest!("../../book/src/contributors.md");
    doc_comment::doctest!("../../book/src/introduction.md");
    doc_comment::doctest!("../../book/src/server.md");
}

mod error;
pub use error::*;

mod address;

mod guid;
pub use guid::*;

mod message;
pub use message::*;

mod message_header;
pub use message_header::*;

mod message_field;
pub use message_field::*;

mod message_fields;
pub use message_fields::*;

mod connection;
pub use connection::*;

mod proxy;
pub use proxy::*;

mod owned_fd;
pub use owned_fd::*;

mod utils;

mod object_server;
pub use object_server::*;

pub mod fdo;

pub mod raw;

pub mod handshake;

pub mod xml;

pub use zbus_macros::{dbus_interface, dbus_proxy, DBusError};

// Required for the macros to function within this crate.
extern crate self as zbus;

// Macro support module, not part of the public API.
#[doc(hidden)]
pub mod export {
    pub use zvariant;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::TryInto;
    use std::fs::File;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    use enumflags2::BitFlags;
    use ntest::timeout;
    use serde_repr::{Deserialize_repr, Serialize_repr};

    use zvariant::{derive::Type, Fd, OwnedValue, Type};

    use crate::{Connection, Message, MessageFlags};

    #[test]
    fn msg() {
        let mut m = Message::method(
            None,
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Peer"),
            "GetMachineId",
            &(),
        )
        .unwrap();
        m.modify_primary_header(|primary| {
            primary.set_flags(BitFlags::from(MessageFlags::NoAutoStart));
            primary.set_serial_num(11);

            Ok(())
        })
        .unwrap();
        let primary = m.primary_header().unwrap();
        assert!(primary.serial_num() == 11);
        assert!(primary.flags() == MessageFlags::NoAutoStart);
    }

    #[test]
    fn basic_connection() {
        let connection = crate::Connection::new_session()
            .map_err(|e| {
                println!("error: {}", e);

                e
            })
            .unwrap();
        // Hello method is already called during connection creation so subsequent calls are expected to fail but only
        // with a D-Bus error.
        match connection.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "Hello",
            &(),
        ) {
            Err(crate::Error::MethodError(_, _, _)) => (),
            Err(e) => panic!("{}", e),
            _ => panic!(),
        };
    }

    #[test]
    fn fdpass_systemd() {
        let connection = crate::Connection::new_system().unwrap();

        let mut reply = connection
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "DumpByFileDescriptor",
                &(),
            )
            .unwrap();

        assert!(reply
            .body_signature()
            .map(|s| s == <Fd>::signature())
            .unwrap());

        let fd: Fd = reply.body().unwrap();
        reply.disown_fds();
        assert!(fd.as_raw_fd() >= 0);
        let f = unsafe { File::from_raw_fd(fd.as_raw_fd()) };
        f.metadata().unwrap();
    }

    #[test]
    fn freedesktop_api() {
        let mut connection = crate::Connection::new_session()
            .map_err(|e| {
                println!("error: {}", e);

                e
            })
            .unwrap();

        connection.set_default_message_handler(Box::new(|msg| {
            // Debug implementation will test it a bit
            println!("Received while waiting for a reply: {}", msg);

            Some(msg)
        }));

        // Let's try getting us a fancy name on the bus
        #[repr(u32)]
        #[derive(Type, BitFlags, Debug, PartialEq, Copy, Clone)]
        enum RequestNameFlags {
            AllowReplacement = 0x01,
            ReplaceExisting = 0x02,
            DoNotQueue = 0x04,
        }

        #[repr(u32)]
        #[derive(Deserialize_repr, Serialize_repr, Type, Debug, PartialEq)]
        enum RequestNameReply {
            PrimaryOwner = 0x01,
            InQueue = 0x02,
            Exists = 0x03,
            AlreadyOwner = 0x04,
        }

        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "RequestName",
                &(
                    "org.freedesktop.zbus",
                    BitFlags::from(RequestNameFlags::ReplaceExisting),
                ),
            )
            .unwrap();

        assert!(reply.body_signature().map(|s| s == "u").unwrap());
        let reply: RequestNameReply = reply.body().unwrap();
        assert_eq!(reply, RequestNameReply::PrimaryOwner);

        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetId",
                &(),
            )
            .unwrap();

        assert!(reply
            .body_signature()
            .map(|s| s == <&str>::signature())
            .unwrap());
        let id: &str = reply.body().unwrap();
        println!("Unique ID of the bus: {}", id);

        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &"org.freedesktop.zbus",
            )
            .unwrap();

        assert!(reply
            .body_signature()
            .map(|s| s == bool::signature())
            .unwrap());
        assert!(reply.body::<bool>().unwrap());

        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &"org.freedesktop.zbus",
            )
            .unwrap();

        assert!(reply
            .body_signature()
            .map(|s| s == <&str>::signature())
            .unwrap());
        assert_eq!(
            Some(reply.body::<&str>().unwrap()),
            connection.unique_name()
        );

        let reply = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionCredentials",
                &"org.freedesktop.DBus",
            )
            .unwrap();

        assert!(reply.body_signature().map(|s| s == "a{sv}").unwrap());
        let hashmap: HashMap<&str, OwnedValue> = reply.body().unwrap();

        let pid: u32 = (&hashmap["ProcessID"]).try_into().unwrap();
        println!("DBus bus PID: {}", pid);

        let uid: u32 = (&hashmap["UnixUserID"]).try_into().unwrap();
        println!("DBus bus UID: {}", uid);
    }

    #[test]
    #[timeout(1000)]
    fn issue_68() {
        // Tests the fix for https://gitlab.freedesktop.org/zeenix/zbus/-/issues/68
        //
        // While this is not an exact reproduction of the issue 68, the underlying problem it
        // produces is exactly the same: `Connection::call_method` dropping all incoming messages
        // while waiting for the reply to the method call.
        let conn = Connection::new_session().unwrap();

        // Send a message as client before service starts to process messages
        let client_conn = Connection::new_session().unwrap();
        let msg = Message::method(
            None,
            conn.unique_name(),
            "/org/freedesktop/Issue68",
            Some("org.freedesktop.Issue68"),
            "Ping",
            &(),
        )
        .unwrap();
        let serial = client_conn.send_message(msg).unwrap();

        crate::fdo::DBusProxy::new(&conn).unwrap().get_id().unwrap();

        loop {
            let msg = conn.receive_message().unwrap();

            if msg.primary_header().unwrap().serial_num() == serial {
                break;
            }
        }
    }
}
