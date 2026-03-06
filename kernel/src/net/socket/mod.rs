//! Sockets are kept as close to the original BSD definition as makes sense.
//! They are a way to stream data to and from a remote port, using either TCP
//! or UDP protocols.
//! Sockets also naturally fit into the AsyncIO semantics of IDOS.
//! First, a socket is created. This just generates a handle with no changes
//! within the net stack.
//! Next, the socket is bound, just like binding a file handle to a path. A
//! socket can either be bound locally (eg, 0.0.0.0:8080), or remotely.
//!
//! For UDP Sockets:
//! If the port was local and not already in use, the socket can issue `read`
//! IO ops to the socket handle. Each read will block until a packet is received
//! on the port. Each write contains the destionation address and port in the
//! first 6 bytes, followed by the data that is instantly sent.
//!
//! For TCP Sockets:
//! If the port was local, the socket becomes a listener. Issuing a `read` op
//! will block until a connection is made. Once a connection is established,
//! a new socket handle will be generated and attached to the Task. The 32-bit
//! ID of that socket handle will be returned, and the Task can use that the
//! same way as a remote connection.
//! For remote TCP connections, an opened socket will automatically initiate
//! the SYN/ACK handshake. The open/bind operation will block until the
//! handshake is complete. If successful, read/write IO ops will send data to
//! and from the remote location.
//!
//! This means there are three different classes of sockets:
//!  - UDP, bound to a local port
//!  - TCP Listener, bound to a local port
//!  - TCP Connection, with a local and remote address
//! Each of these is represented by a socket handle, and modified through a
//! socket IO provider.

pub mod binding;
pub mod listen;
pub mod port;

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
use idos_api::io::error::IoError;
use spin::RwLock;

use crate::{io::async_io::AsyncOpID, task::id::TaskID};

use self::{
    listen::{TcpListener, UdpListener},
    port::SocketPort,
};
use super::{
    protocol::{
        ipv4::Ipv4Address,
        tcp::{connection::TcpConnection, header::TcpHeader},
    },
    resident::net_open_tcp,
};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SocketId(u32);

impl SocketId {
    pub fn new(id: u32) -> Self {
        Self(id)
    }
}

impl core::ops::Deref for SocketId {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

static NEXT_SOCKET_ID: AtomicU32 = AtomicU32::new(1);

/// Mapping of local ports to sockets. This is used when a new packet arrives,
/// to determine how it should be handled.
/// This only stores UDP and TCP sockets that have been directly bound.
/// TCP connections that were created by accepting an incoming connection are
/// stored within the parent listener's connections map.
static ACTIVE_CONNECTIONS: RwLock<BTreeMap<SocketPort, SocketId>> = RwLock::new(BTreeMap::new());

enum SocketType {
    Udp(UdpListener),
    TcpListener(TcpListener),
    TcpConnection(TcpConnection),
}

/// Map of all active sockets
static SOCKET_MAP: RwLock<BTreeMap<SocketId, SocketType>> = RwLock::new(BTreeMap::new());

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SocketProtocol {
    Udp,
    Tcp,
}

pub type AsyncCallback = (TaskID, u32, AsyncOpID);

pub fn create_dns_socket(port: SocketPort) {
    // This is a special socket that is used for DNS lookups.
    // It's not actually used, it gets intercepted at the highest network layer
    let socket_id = SocketId::new(NEXT_SOCKET_ID.fetch_add(1, Ordering::SeqCst));
    ACTIVE_CONNECTIONS.write().insert(port, socket_id);
}

/// When a task issues a bind operation on a socket handle, this method contains
/// the logic used to create the socket.
/// Like all async IO operations, it returns an optional result. If the op can
/// complete immediately, like creating a local UDP binding, it returns `Some`
/// Result. If the operation must wait for a remote connection, it returns
/// `None` and uses the async callback info to resolve the IO operation when
/// complete.
pub fn socket_io_bind(
    protocol: SocketProtocol,
    addr_string: &[u8],
    port: u16,
    callback: AsyncCallback,
) -> Option<Result<u32, IoError>> {
    match protocol {
        SocketProtocol::Udp => {
            let addr = match Ipv4Address::parse_bytes(addr_string) {
                Some(addr) => addr,
                None => return Some(Err(IoError::InvalidArgument)),
            };
            if addr != Ipv4Address([0, 0, 0, 0]) && addr != Ipv4Address([127, 0, 0, 1]) {
                // We don't currently support declaring which local IP to use,
                // so this is just an error case
                return Some(Err(IoError::InvalidArgument));
            }

            let mut active_connections = ACTIVE_CONNECTIONS.write();
            let port = SocketPort::new(port);
            if active_connections.get(&port).is_some() {
                // Port is already in use
                return Some(Err(IoError::ResourceInUse));
            }

            let socket_id = SocketId::new(NEXT_SOCKET_ID.fetch_add(1, Ordering::SeqCst));
            active_connections.insert(port, socket_id);
            drop(active_connections);

            let listener = UdpListener::new(port);
            SOCKET_MAP
                .write()
                .insert(socket_id, SocketType::Udp(listener));
            Some(Ok(*socket_id))
        }
        SocketProtocol::Tcp => {
            if let Some(addr) = Ipv4Address::parse_bytes(addr_string) {
                if addr == Ipv4Address([0, 0, 0, 0]) || addr == Ipv4Address([127, 0, 0, 1]) {
                    // This is a local TCP listener
                    let mut active_connections = ACTIVE_CONNECTIONS.write();
                    let port = SocketPort::new(port);
                    if active_connections.get(&port).is_some() {
                        return Some(Err(IoError::ResourceInUse));
                    }

                    let socket_id = SocketId::new(NEXT_SOCKET_ID.fetch_add(1, Ordering::SeqCst));
                    active_connections.insert(port, socket_id);
                    drop(active_connections);

                    let listener = TcpListener::new(port);
                    SOCKET_MAP
                        .write()
                        .insert(socket_id, SocketType::TcpListener(listener));
                    return Some(Ok(*socket_id));
                }
            }
            // this is an attempt to connect to a remote TCP address.
            // It may be a known IP, or it may be a domain name that requires
            // a DNS lookup.
            //
            // The process will be asynchronous, and will use the callback info
            // to resolve the op later.
            let remote_port = SocketPort::new(port);
            let local_port = match get_ephemeral_port() {
                Some(p) => p,
                None => return Some(Err(IoError::ResourceLimitExceeded)),
            };
            let socket_id = SocketId::new(NEXT_SOCKET_ID.fetch_add(1, Ordering::SeqCst));
            ACTIVE_CONNECTIONS.write().insert(local_port, socket_id);
            let remote_addr = match Ipv4Address::parse_bytes(addr_string) {
                Some(addr) => addr,
                // It can't be the broadcast address, so we use that as a placeholder
                None => Ipv4Address([255, 255, 255, 255]),
            };
            let conn = TcpConnection::new(
                socket_id,
                local_port,
                remote_addr,
                remote_port,
                true,                    // is outbound
                Some((callback, false)), // callback, not creating provider
            );
            SOCKET_MAP
                .write()
                .insert(socket_id, SocketType::TcpConnection(conn));

            // TODO: this is wasteful, we shouldn't need to build a String
            net_open_tcp(
                local_port,
                String::from_utf8_lossy(addr_string).to_string(),
                remote_port,
            );

            None
        }
    }
}

pub fn get_ephemeral_port() -> Option<SocketPort> {
    let mut test_port = 49152;
    while test_port < 65535 {
        let port = SocketPort::new(test_port);
        {
            let active_connections = ACTIVE_CONNECTIONS.read();
            if !active_connections.contains_key(&port) {
                return Some(port); // Found an available ephemeral port
            }
        }
        test_port += 1;
    }
    None
}

/// Depending on the socket type, this may either
/// 1) blocking read on a UDP port
/// 2) wait for connection on a TCP listener
/// 3) blocking read from an established TCP connection
pub fn socket_io_read(
    socket_id: SocketId,
    buffer: &mut [u8],
    callback: AsyncCallback,
) -> Option<Result<u32, IoError>> {
    let mut socket_map = SOCKET_MAP.write();
    let socket_type = match socket_map.get_mut(&socket_id) {
        Some(socket_type) => socket_type,
        None => return Some(Err(IoError::FileHandleInvalid)),
    };
    match socket_type {
        SocketType::Udp(listener) => listener.read(buffer, callback),
        SocketType::TcpListener(listener) => listener.accept(buffer, callback),
        SocketType::TcpConnection(connection) => connection.read(buffer, callback),
    }
}

/// Depending on the socket type, this may either
/// 1) write from a UDP port, using a remote address in the write buffer
/// 2) error because it tried to write to a TCP listener
/// 3) write to an established TCP connection
pub fn socket_io_write(
    socket_id: SocketId,
    buffer: &[u8],
    callback: AsyncCallback,
) -> Option<Result<u32, IoError>> {
    let mut socket_map = SOCKET_MAP.write();
    let socket_type = match socket_map.get_mut(&socket_id) {
        Some(socket_type) => socket_type,
        None => return Some(Err(IoError::FileHandleInvalid)),
    };
    match socket_type {
        SocketType::Udp(listener) => {
            let local_ip = listen::get_resolved_local_ip();
            listener.write(buffer, local_ip, callback)
        }
        SocketType::TcpListener(_) => Some(Err(IoError::UnsupportedOperation)),
        SocketType::TcpConnection(connection) => connection.write(buffer),
    }
}

pub fn handle_udp_packet(local_port: u16, remote_addr: Ipv4Address, remote_port: u16, data: &[u8]) {
    let port = SocketPort::new(local_port);
    let socket_id = {
        let active_connections = ACTIVE_CONNECTIONS.read();
        match active_connections.get(&port) {
            Some(id) => *id,
            None => return, // No listener for this port
        }
    };

    let mut socket_map = SOCKET_MAP.write();
    if let Some(socket_type) = socket_map.get_mut(&socket_id) {
        if let SocketType::Udp(listener) = socket_type {
            listener.handle_packet(remote_addr, remote_port, data);
        }
    }
}

pub fn handle_tcp_packet(
    local_addr: Ipv4Address,
    local_port: u16,
    remote_addr: Ipv4Address,
    tcp_header: &TcpHeader,
    data: &[u8],
) {
    let port = SocketPort::new(local_port);
    let socket_id = {
        let active_connections = ACTIVE_CONNECTIONS.read();
        match active_connections.get(&port) {
            Some(id) => *id,
            None => return, // No listener for this port
        }
    };

    let mut socket_map = SOCKET_MAP.write();
    let mut lookup_id = socket_id;
    if let Some(SocketType::TcpListener(listener)) = socket_map.get_mut(&socket_id) {
        let remote_port = SocketPort::new(u16::from_be(tcp_header.source_port));
        if let Some(conn_id) = listener.connections.find(remote_addr, remote_port) {
            lookup_id = conn_id;
        }
    }
    if let Some(socket_type) = socket_map.get_mut(&lookup_id) {
        match socket_type {
            SocketType::TcpListener(listener) => {
                if let Some((new_conn_id, new_conn)) =
                    listener.handle_packet(local_addr, remote_addr, tcp_header, data)
                {
                    // the new connection needs to be passed back, since we're
                    // holding the socket map lock
                    socket_map.insert(new_conn_id, SocketType::TcpConnection(new_conn));
                }
            }
            SocketType::TcpConnection(connection) => {
                connection.handle_packet(local_addr, remote_addr, tcp_header, data);
            }
            _ => {}
        }
    }
}
