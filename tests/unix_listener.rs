#![cfg(all(unix, feature = "os-poll", feature = "net"))]

use mio::net::UnixListener;
use mio::{Interest, Token};
use std::io::{self, Read};
use std::os::unix::net;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

#[macro_use]
mod util;
use util::{
    assert_send, assert_socket_close_on_exec, assert_socket_non_blocking, assert_sync,
    assert_would_block, expect_events, expect_no_events, init_with_poll, temp_file, ExpectEvent,
};

const DEFAULT_BUF_SIZE: usize = 64;
const TOKEN_1: Token = Token(0);

#[test]
fn unix_listener_send_and_sync() {
    assert_send::<UnixListener>();
    assert_sync::<UnixListener>();
}

#[test]
fn unix_listener_smoke() {
    #[allow(clippy::redundant_closure)]
    smoke_test(|path| UnixListener::bind(path), "unix_listener_smoke");
}

#[test]
fn unix_listener_from_std() {
    smoke_test(
        |path| {
            let listener = net::UnixListener::bind(path).unwrap();
            // `std::os::unix::net::UnixStream`s are blocking by default, so make sure
            // it is in non-blocking mode before wrapping in a Mio equivalent.
            listener.set_nonblocking(true).unwrap();
            Ok(UnixListener::from_std(listener))
        },
        "unix_listener_from_std",
    )
}

#[test]
fn unix_listener_local_addr() {
    let (mut poll, mut events) = init_with_poll();
    let barrier = Arc::new(Barrier::new(2));

    let path = temp_file("unix_listener_local_addr");
    let mut listener = UnixListener::bind(&path).unwrap();
    poll.registry()
        .register(
            &mut listener,
            TOKEN_1,
            Interest::WRITABLE.add(Interest::READABLE),
        )
        .unwrap();

    let handle = open_connections(path.clone(), 1, barrier.clone());
    expect_events(
        &mut poll,
        &mut events,
        vec![ExpectEvent::new(TOKEN_1, Interest::READABLE)],
    );

    let (stream, expected_addr) = listener.accept().unwrap();
    // getting pathname isn't supported on GNU/Hurd
    #[cfg(not(target_os = "hurd"))]
    assert_eq!(stream.local_addr().unwrap().as_pathname().unwrap(), &path);
    assert!(expected_addr.as_pathname().is_none());

    barrier.wait();
    handle.join().unwrap();
}

#[test]
fn unix_listener_register() {
    let (mut poll, mut events) = init_with_poll();

    let path = temp_file("unix_listener_register");
    let mut listener = UnixListener::bind(path).unwrap();
    poll.registry()
        .register(&mut listener, TOKEN_1, Interest::READABLE)
        .unwrap();
    expect_no_events(&mut poll, &mut events)
}

#[test]
fn unix_listener_reregister() {
    let (mut poll, mut events) = init_with_poll();
    let barrier = Arc::new(Barrier::new(2));

    let path = temp_file("unix_listener_reregister");
    let mut listener = UnixListener::bind(&path).unwrap();
    poll.registry()
        .register(&mut listener, TOKEN_1, Interest::WRITABLE)
        .unwrap();

    let handle = open_connections(path, 1, barrier.clone());
    expect_no_events(&mut poll, &mut events);

    poll.registry()
        .reregister(&mut listener, TOKEN_1, Interest::READABLE)
        .unwrap();
    expect_events(
        &mut poll,
        &mut events,
        vec![ExpectEvent::new(TOKEN_1, Interest::READABLE)],
    );
    // Complete handshake to unblock the client thread.
    #[cfg(target_os = "cygwin")]
    listener.accept().unwrap();

    barrier.wait();
    handle.join().unwrap();
}

#[test]
fn unix_listener_deregister() {
    let (mut poll, mut events) = init_with_poll();
    let barrier = Arc::new(Barrier::new(2));

    let path = temp_file("unix_listener_deregister");
    let mut listener = UnixListener::bind(&path).unwrap();
    poll.registry()
        .register(&mut listener, TOKEN_1, Interest::READABLE)
        .unwrap();

    let handle = open_connections(path, 1, barrier.clone());

    poll.registry().deregister(&mut listener).unwrap();
    expect_no_events(&mut poll, &mut events);
    // Complete handshake to unblock the client thread.
    #[cfg(target_os = "cygwin")]
    listener.accept().unwrap();

    barrier.wait();
    handle.join().unwrap();
}

#[test]
#[cfg(any(target_os = "android", target_os = "linux"))]
fn unix_listener_abstract_namespace() {
    use rand::Rng;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let (mut poll, mut events) = init_with_poll();
    let barrier = Arc::new(Barrier::new(2));

    let num: u64 = rand::thread_rng().gen();
    let name = format!("mio-abstract-uds-{}", num);
    let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let mut listener = UnixListener::bind_addr(&address).unwrap();
    assert_eq!(
        listener.local_addr().unwrap().as_abstract_name(),
        address.as_abstract_name(),
    );

    poll.registry()
        .register(
            &mut listener,
            TOKEN_1,
            Interest::WRITABLE.add(Interest::READABLE),
        )
        .unwrap();
    expect_no_events(&mut poll, &mut events);

    let barrier2 = barrier.clone();
    let handle = thread::spawn(move || {
        let conn = net::UnixStream::connect_addr(&address).unwrap();
        barrier2.wait();
        drop(conn);
    });
    expect_events(
        &mut poll,
        &mut events,
        vec![ExpectEvent::new(TOKEN_1, Interest::READABLE)],
    );

    let (_, address) = listener.accept().unwrap();
    assert!(address.is_unnamed());

    barrier.wait();
    handle.join().unwrap();
}

fn smoke_test<F>(new_listener: F, test_name: &'static str)
where
    F: FnOnce(&Path) -> io::Result<UnixListener>,
{
    let (mut poll, mut events) = init_with_poll();
    let barrier = Arc::new(Barrier::new(2));
    let path = temp_file(test_name);

    let mut listener = new_listener(&path).unwrap();

    assert_socket_non_blocking(&listener);
    assert_socket_close_on_exec(&listener);

    poll.registry()
        .register(
            &mut listener,
            TOKEN_1,
            Interest::WRITABLE.add(Interest::READABLE),
        )
        .unwrap();
    expect_no_events(&mut poll, &mut events);

    let handle = open_connections(path, 1, barrier.clone());
    expect_events(
        &mut poll,
        &mut events,
        vec![ExpectEvent::new(TOKEN_1, Interest::READABLE)],
    );

    let (mut stream, _) = listener.accept().unwrap();

    let mut buf = [0; DEFAULT_BUF_SIZE];
    assert_would_block(stream.read(&mut buf));

    assert_would_block(listener.accept());
    assert!(listener.take_error().unwrap().is_none());

    barrier.wait();
    handle.join().unwrap();
}

fn open_connections(
    path: PathBuf,
    n_connections: usize,
    barrier: Arc<Barrier>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for _ in 0..n_connections {
            let conn = net::UnixStream::connect(path.clone()).unwrap();
            barrier.wait();
            drop(conn);
        }
    })
}
