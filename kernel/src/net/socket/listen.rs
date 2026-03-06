use core::sync::atomic::Ordering;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use idos_api::io::error::{IoError, IoResult};

use crate::task::map::get_task;

use super::{
    super::protocol::{
        ipv4::Ipv4Address,
        tcp::{connection::TcpConnection, header::TcpHeader},
        udp::create_datagram,
    },
    super::resident::net_respond,
    port::SocketPort,
    AsyncCallback, SocketId, SocketType,
};

pub struct UdpListener {
    port: SocketPort,
}

impl UdpListener {
    pub fn new(port: SocketPort) -> Self {
        Self { port }
    }

    pub fn handle_packet(&self, _remote_addr: Ipv4Address, _remote_port: u16, _data: &[u8]) {}

    /// Block until the next packet is received on this UDP listener.
    /// If the packet is open, incoming reads will be queued up and can be
    /// immediately resolved. Otherwise, the method will return and the next
    /// incoming packet will use the async callback info to resolve the read
    /// operation.
    pub fn read(&self, _buffer: &mut [u8], _callback: AsyncCallback) -> Option<IoResult> {
        Some(Err(IoError::Unknown))
    }

    /// Write a UDP datagram. The buffer format is:
    ///   [dest_ip: 4 bytes] [dest_port: 2 bytes, big-endian] [payload...]
    /// If the local IP is known, the datagram is sent immediately and the
    /// callback is completed synchronously. Otherwise, the write is queued
    /// in PENDING_UDP_WRITES and will be flushed when DHCP completes.
    pub fn write(&self, buffer: &[u8], local_ip: Option<Ipv4Address>, callback: AsyncCallback) -> Option<IoResult> {
        if buffer.len() < 6 {
            return Some(Err(IoError::InvalidArgument));
        }
        let dest_ip = Ipv4Address([buffer[0], buffer[1], buffer[2], buffer[3]]);
        let dest_port = u16::from_be_bytes([buffer[4], buffer[5]]);
        let payload = &buffer[6..];

        if let Some(src_ip) = local_ip {
            let datagram = create_datagram(src_ip, *self.port, dest_ip, dest_port, payload);
            net_respond(dest_ip, datagram);
            complete_op(callback, Ok(payload.len() as u32));
            Some(Ok(payload.len() as u32))
        } else {
            // Queue for later — DHCP hasn't resolved yet
            PENDING_UDP_WRITES.lock().push_back(PendingUdpWrite {
                source_port: self.port,
                dest_ip,
                dest_port,
                payload: Vec::from(payload),
                callback,
            });
            None
        }
    }
}

pub struct PendingUdpWrite {
    pub source_port: SocketPort,
    pub dest_ip: Ipv4Address,
    pub dest_port: u16,
    pub payload: Vec<u8>,
    pub callback: AsyncCallback,
}

// TODO: These are global singletons, which only works with a single network
// device. When multi-device support is added, the pending queue and resolved IP
// should be per-device.
static PENDING_UDP_WRITES: spin::Mutex<VecDeque<PendingUdpWrite>> =
    spin::Mutex::new(VecDeque::new());

static RESOLVED_LOCAL_IP: spin::RwLock<Option<Ipv4Address>> = spin::RwLock::new(None);

/// Returns the local IP if DHCP has completed.
pub fn get_resolved_local_ip() -> Option<Ipv4Address> {
    *RESOLVED_LOCAL_IP.read()
}

/// Called when DHCP completes and the local IP is known.
/// Stores the IP for future synchronous access, then flushes all pending UDP writes.
pub fn flush_pending_udp_writes(local_ip: Ipv4Address) {
    *RESOLVED_LOCAL_IP.write() = Some(local_ip);
    let mut pending = PENDING_UDP_WRITES.lock();
    while let Some(write) = pending.pop_front() {
        let datagram = create_datagram(
            local_ip,
            *write.source_port,
            write.dest_ip,
            write.dest_port,
            &write.payload,
        );
        net_respond(write.dest_ip, datagram);
        complete_op(write.callback, Ok(write.payload.len() as u32));
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct RemoteEndpoint {
    pub address: Ipv4Address,
    pub port: SocketPort,
}

pub struct ListenerConnections {
    lookup: BTreeMap<RemoteEndpoint, SocketId>,
}

impl ListenerConnections {
    pub fn new() -> Self {
        Self {
            lookup: BTreeMap::new(),
        }
    }

    pub fn add(
        &mut self,
        remote_address: Ipv4Address,
        remote_port: SocketPort,
        socket_id: SocketId,
    ) {
        let endpoint = RemoteEndpoint {
            address: remote_address,
            port: remote_port,
        };
        self.lookup.insert(endpoint, socket_id);
    }

    pub fn remove(
        &mut self,
        remote_address: Ipv4Address,
        remote_port: SocketPort,
    ) -> Option<SocketId> {
        let endpoint = RemoteEndpoint {
            address: remote_address,
            port: remote_port,
        };
        self.lookup.remove(&endpoint)
    }

    pub fn find(&self, remote_address: Ipv4Address, remote_port: SocketPort) -> Option<SocketId> {
        let endpoint = RemoteEndpoint {
            address: remote_address,
            port: remote_port,
        };
        self.lookup.get(&endpoint).copied()
    }
}

pub struct TcpListener {
    local_port: SocketPort,
    pub connections: ListenerConnections,
    pending_syn: VecDeque<(Ipv4Address, SocketPort)>,
    pending_accept: VecDeque<AsyncCallback>,
}

impl TcpListener {
    pub fn new(port: SocketPort) -> Self {
        Self {
            local_port: port,
            connections: ListenerConnections::new(),
            pending_syn: VecDeque::new(),
            pending_accept: VecDeque::new(),
        }
    }

    pub fn handle_packet(
        &mut self,
        local_addr: Ipv4Address,
        remote_addr: Ipv4Address,
        tcp_header: &TcpHeader,
        data: &[u8],
    ) -> Option<(SocketId, TcpConnection)> {
        let remote_port = SocketPort::new(u16::from_be(tcp_header.source_port));
        match self.connections.find(remote_addr, remote_port) {
            Some(_) => panic!(),
            None => {
                if tcp_header.is_syn() {
                    // If the packet is a SYN, we queue it for later processing
                    if self.pending_accept.is_empty() {
                        self.pending_syn.push_back((remote_addr, remote_port));
                    } else {
                        // If we have a pending accept, we can immediately process the SYN
                        let callback = self.pending_accept.pop_front().unwrap();
                        return Some(self.init_connection(
                            local_addr,
                            remote_addr,
                            remote_port,
                            u32::from_be(tcp_header.sequence_number),
                            callback,
                        ));
                    }
                }
            }
        }
        None
    }

    pub fn init_connection(
        &mut self,
        local_addr: Ipv4Address,
        remote_addr: Ipv4Address,
        remote_port: SocketPort,
        last_seq: u32,
        callback: AsyncCallback,
    ) -> (SocketId, TcpConnection) {
        let is_outbound = last_seq == 0;
        let socket_id = SocketId::new(super::NEXT_SOCKET_ID.fetch_add(1, Ordering::SeqCst));
        let mut connection = TcpConnection::new(
            socket_id,
            self.local_port,
            remote_addr,
            remote_port,
            is_outbound,
            Some((callback, !is_outbound)),
        );
        connection.last_sequence_received = last_seq;
        self.connections.add(remote_addr, remote_port, socket_id);
        let flags = if is_outbound {
            TcpHeader::FLAG_SYN
        } else {
            TcpHeader::FLAG_SYN | TcpHeader::FLAG_ACK
        };
        let response = TcpHeader::create_packet(
            local_addr,
            self.local_port,
            remote_addr,
            remote_port,
            connection.last_sequence_sent,
            connection.last_sequence_received + 1,
            flags,
            &[],
        );
        net_respond(remote_addr, response);

        (socket_id, connection)
    }

    /// Accept a new connection on this TCP listener.
    /// Incoming SYN packets are queued. An accept call will complete the
    /// handshake. Regardless of whether a connection has been initiated before
    /// this method is called, it will always be an async process and will
    /// never immediately return an `IoResult>.
    pub fn accept(&mut self, buffer: &mut [u8], callback: AsyncCallback) -> Option<IoResult> {
        if self.pending_syn.is_empty() {
            self.pending_accept.push_back(callback);
            return None;
        }
        unimplemented!();
        None
    }
}

pub fn complete_op(callback: AsyncCallback, result: IoResult) {
    let (task_id, io_index, op_id) = callback;
    let task_lock = match get_task(task_id) {
        Some(lock) => lock,
        None => return,
    };
    let io_entry = task_lock.read().async_io_complete(io_index);
    if let Some(entry) = io_entry {
        entry.inner().async_complete(op_id, result);
    }
}
