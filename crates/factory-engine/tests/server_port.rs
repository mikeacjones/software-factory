//! The port a test's local Temporal server is started on stays its own
//! until the server takes it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::net::TcpListener;

/// More sockets than any system's range of ports given to those that ask
/// for any port, so the kernel's cursor goes all the way round it.
const PAST_THE_WHOLE_RANGE: usize = 70_000;

/// The kernel's cursor over the ports it hands out can be anywhere: on
/// macOS a socket that asks for a random port moves it at random. Wherever
/// it is, a port reserved for a server is not handed to anyone else, and
/// the server can still bind it by number.
#[test]
fn a_port_reserved_for_a_server_is_not_handed_to_another_socket_before_it_listens() {
    let port = common::reserved_port();
    for _ in 0..PAST_THE_WHOLE_RANGE {
        let Ok(other) = TcpListener::bind("127.0.0.1:0") else {
            continue;
        };
        let given = other.local_addr().unwrap().port();
        assert_ne!(
            given, port,
            "the port reserved for the server was handed to another socket"
        );
    }
    TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|err| panic!("the server could not take port {port}: {err}"));
}
