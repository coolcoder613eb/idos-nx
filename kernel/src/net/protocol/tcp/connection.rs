use alloc::collections::VecDeque;
use alloc::vec::Vec;
use idos_api::io::error::{IoError, IoResult};

use crate::io::async_io::IOType;
use crate::io::provider::socket::SocketIOProvider;
use crate::io::provider::IOProvider;
use crate::memory::address::{PhysicalAddress, VirtualAddress};
use crate::memory::virt::scratch::UnmappedPage;
use crate::net::resident::net_respond;
use crate::net::socket::listen::complete_op;
use crate::net::socket::AsyncCallback;
use crate::task::map::get_task;
use crate::task::paging::get_current_physical_address;

use super::super::super::socket::{port::SocketPort, SocketId};
use super::super::{ipv4::Ipv4Address, packet::PacketHeader};
use super::header::TcpHeader;

#[derive(Clone, Copy)]
pub enum TcpState {
    /// Outbound connection is being established
    SynSent,
    /// Inbound connection is being established
    SynReceived,
    /// Connection is established and ready for data transfer
    Established,
    /// Received a FIN packet, waiting for ACK
    LastAck,
}

/// The TCPAction enum encodes a number of actions that should be performed
/// when a TCP packet is received. Because TCP is basically a state machine,
/// there are a number of cases where a received packet triggers some new
/// behavior beyond the core send/receive loop
pub enum TcpAction {
    /// Send the packet to be read from the socket
    Enqueue,
    /// Throw away the packet, ie in case of a duplicate
    Discard,
    /// Close the socket without sending anything
    Close,
    /// Send a RST packet and close the connection
    Reset,
    /// Send a FIN/ACK to close
    FinAck,
    /// Mark the connection as established
    Connect,
    ConnectAck,
}

struct PendingRead {
    buffer_paddr: PhysicalAddress,
    buffer_len: usize,
    callback: AsyncCallback,
}

pub struct TcpConnection {
    own_id: SocketId,
    local_address: Ipv4Address,
    local_port: SocketPort,
    remote_address: Ipv4Address,
    remote_port: SocketPort,
    state: TcpState,
    pub last_sequence_sent: u32,
    pub last_sequence_received: u32,

    on_connect: Option<(AsyncCallback, bool)>,
    pending_reads: VecDeque<PendingRead>,
    available_data: Vec<u8>,
}

impl TcpConnection {
    pub fn new(
        own_id: SocketId,
        local_port: SocketPort,
        remote_address: Ipv4Address,
        remote_port: SocketPort,
        is_outbound: bool,
        on_connect: Option<(AsyncCallback, bool)>,
    ) -> Self {
        Self {
            own_id,
            local_address: Ipv4Address([0, 0, 0, 0]),
            local_port,
            remote_address,
            remote_port,
            state: if is_outbound {
                TcpState::SynSent
            } else {
                TcpState::SynReceived
            },
            last_sequence_sent: 0,
            last_sequence_received: 0,
            on_connect,
            pending_reads: VecDeque::new(),
            available_data: Vec::new(),
        }
    }

    pub fn handle_packet(
        &mut self,
        local_addr: Ipv4Address,
        remote_addr: Ipv4Address,
        header: &TcpHeader,
        data: &[u8],
    ) {
        let action = self.action_for_tcp_packet(header);
        let packet_to_send = match action {
            TcpAction::Close => {
                // Complete any pending reads with Ok(0) to unblock waiting tasks
                while let Some(read) = self.pending_reads.pop_front() {
                    complete_op(read.callback, Ok(0));
                }
                // TODO: the socket connection needs to be cleaned up
                None
            }
            TcpAction::Connect | TcpAction::ConnectAck => {
                self.state = TcpState::Established;
                self.local_address = local_addr;
                self.remote_address = remote_addr;
                self.last_sequence_sent += 1;
                self.last_sequence_received = u32::from_be(header.sequence_number) + 1;
                if let Some((callback, should_create_provider)) = self.on_connect.take() {
                    if should_create_provider {
                        let mut provider = SocketIOProvider::create_tcp();
                        provider.bind_to(*self.own_id);
                        let task_lock = match get_task(callback.0) {
                            Some(task) => task,
                            None => return,
                        };
                        let mut task_guard = task_lock.write();
                        let io_index = task_guard.async_io_table.add_io(IOType::Socket(provider));
                        let new_handle = task_guard.open_handles.insert(io_index);
                        drop(task_guard);
                        complete_op(callback, Ok(*new_handle as u32));
                    } else {
                        complete_op(callback, Ok(*self.own_id));
                    }
                }
                if let TcpAction::ConnectAck = action {
                    // If we established the connection, we need to send an ACK
                    Some(TcpHeader::create_packet(
                        local_addr,
                        self.local_port,
                        remote_addr,
                        self.remote_port,
                        self.last_sequence_sent,
                        u32::from_be(header.sequence_number) + 1,
                        TcpHeader::FLAG_ACK,
                        &[],
                    ))
                } else {
                    None
                }
            }
            TcpAction::Discard => None,
            TcpAction::Enqueue => {
                if data.is_empty() {
                    // Pure ACK — nothing to enqueue, don't respond
                    return;
                }
                if self.pending_reads.is_empty() {
                    self.available_data.extend_from_slice(data);
                } else {
                    // copy the buffer directly to the read buffer
                    let read = self.pending_reads.pop_front().unwrap();
                    let buffer_offset = read.buffer_paddr.as_u32() & 0xfff;
                    let mapping = UnmappedPage::map(read.buffer_paddr & 0xfffff000);
                    let buffer_ptr = (mapping.virtual_address() + buffer_offset).as_ptr_mut::<u8>();
                    let page_remaining = 0x1000 - buffer_offset as usize;
                    let usable_len = read.buffer_len.min(page_remaining);
                    let buffer =
                        unsafe { core::slice::from_raw_parts_mut(buffer_ptr, usable_len) };
                    let write_length = data.len().min(buffer.len());

                    buffer[..write_length].copy_from_slice(&data[..write_length]);

                    // Save any data that didn't fit for the next read
                    if write_length < data.len() {
                        self.available_data.extend_from_slice(&data[write_length..]);
                    }
                    complete_op(read.callback, Ok(write_length as u32));
                }

                self.last_sequence_received = u32::from_be(header.sequence_number) + data.len() as u32;

                Some(TcpHeader::create_packet(
                    local_addr,
                    self.local_port,
                    remote_addr,
                    self.remote_port,
                    self.last_sequence_sent,
                    self.last_sequence_received,
                    TcpHeader::FLAG_ACK,
                    &[],
                ))
            }
            TcpAction::FinAck => {
                // Deliver any data payload that came with the FIN
                if !data.is_empty() {
                    if self.pending_reads.is_empty() {
                        self.available_data.extend_from_slice(data);
                    } else {
                        let read = self.pending_reads.pop_front().unwrap();
                        let buffer_offset = read.buffer_paddr.as_u32() & 0xfff;
                        let mapping = UnmappedPage::map(read.buffer_paddr & 0xfffff000);
                        let buffer_ptr = (mapping.virtual_address() + buffer_offset).as_ptr_mut::<u8>();
                        let page_remaining = 0x1000 - buffer_offset as usize;
                        let usable_len = read.buffer_len.min(page_remaining);
                        let buffer =
                            unsafe { core::slice::from_raw_parts_mut(buffer_ptr, usable_len) };
                        let write_length = data.len().min(buffer.len());
                        buffer[..write_length].copy_from_slice(&data[..write_length]);
                        if write_length < data.len() {
                            self.available_data.extend_from_slice(&data[write_length..]);
                        }
                        complete_op(read.callback, Ok(write_length as u32));
                    }
                    self.last_sequence_received = u32::from_be(header.sequence_number) + data.len() as u32;
                }

                self.state = TcpState::LastAck;
                // Complete any remaining pending reads with Ok(0) to signal EOF
                while let Some(read) = self.pending_reads.pop_front() {
                    complete_op(read.callback, Ok(0));
                }
                Some(TcpHeader::create_packet(
                    local_addr,
                    self.local_port,
                    remote_addr,
                    self.remote_port,
                    self.last_sequence_sent,
                    self.last_sequence_received + 1,
                    TcpHeader::FLAG_FIN | TcpHeader::FLAG_ACK,
                    &[],
                ))
            }
            TcpAction::Reset => {
                Some(TcpHeader::create_packet(
                    local_addr,
                    self.local_port,
                    remote_addr,
                    self.remote_port,
                    self.last_sequence_sent,
                    u32::from_be(header.sequence_number) + 1,
                    TcpHeader::FLAG_RST | TcpHeader::FLAG_ACK,
                    &[],
                ))
            }
        };

        if let Some(packet) = packet_to_send {
            net_respond(remote_addr, packet);
        }
    }

    /// Determine the action to take based on the current TCP state and the
    /// incoming packet.
    pub fn action_for_tcp_packet(&self, header: &TcpHeader) -> TcpAction {
        if header.is_rst() {
            // no matter what state the connection is in, a reset closes it
            return TcpAction::Close;
        }
        match self.state {
            TcpState::SynSent => {
                if !header.is_syn() || !header.is_ack() {
                    return TcpAction::Reset;
                }
                TcpAction::ConnectAck
            }
            TcpState::SynReceived => {
                if !header.is_ack() {
                    return TcpAction::Reset;
                }
                let ack = u32::from_be(header.ack_number);
                if ack != self.last_sequence_sent + 1 {
                    return TcpAction::Reset;
                }
                TcpAction::Connect
            }

            TcpState::Established => {
                if header.is_fin() {
                    return TcpAction::FinAck;
                }
                if header.is_syn() {
                    return TcpAction::Reset;
                }
                TcpAction::Enqueue
            }

            TcpState::LastAck => TcpAction::Close,
        }
    }

    const MSS: usize = 1460;

    pub fn write(&mut self, data: &[u8]) -> Option<IoResult> {
        if !matches!(self.state, TcpState::Established) {
            return Some(Err(IoError::OperationFailed));
        }

        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + Self::MSS).min(data.len());
            let chunk = &data[offset..end];
            let is_last = end == data.len();

            let flags = if is_last {
                TcpHeader::FLAG_ACK | TcpHeader::FLAG_PSH
            } else {
                TcpHeader::FLAG_ACK
            };

            let packet = TcpHeader::create_packet(
                self.local_address,
                self.local_port,
                self.remote_address,
                self.remote_port,
                self.last_sequence_sent,
                self.last_sequence_received,
                flags,
                chunk,
            );
            self.last_sequence_sent += chunk.len() as u32;
            net_respond(self.remote_address, packet);
            offset = end;
        }

        Some(Ok(data.len() as u32))
    }

    pub fn read(&mut self, buffer: &mut [u8], callback: AsyncCallback) -> Option<IoResult> {
        if !self.available_data.is_empty() {
            let copy_len = buffer.len().min(self.available_data.len());
            buffer[..copy_len].copy_from_slice(&self.available_data[..copy_len]);
            self.available_data.drain(..copy_len);
            return Some(Ok(copy_len as u32));
        }
        if matches!(self.state, TcpState::LastAck) {
            return Some(Ok(0));
        }
        let buffer_vaddr = VirtualAddress::new(buffer.as_ptr() as u32);
        let buffer_paddr = get_current_physical_address(buffer_vaddr).unwrap();
        self.pending_reads.push_back(PendingRead {
            buffer_paddr,
            buffer_len: buffer.len(),
            callback,
        });
        None
    }
}
