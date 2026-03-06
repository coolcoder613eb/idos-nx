use core::sync::atomic::Ordering;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    vec::Vec,
};
use idos_api::io::{AsyncOp, ASYNC_OP_OPEN, ASYNC_OP_READ, ASYNC_OP_WRITE};

use crate::{
    io::handle::Handle,
    task::actions::{handle::create_file_handle, io::send_io_op},
};

use super::{
    protocol::{
        dhcp::{DhcpPacket, DhcpState, IpResolution},
        dns::{get_dns_port, handle_dns_packet},
        ethernet::EthernetFrameHeader,
        ipv4::{IpProtocolType, Ipv4Address, Ipv4Header},
        packet::PacketHeader,
        tcp::header::TcpHeader,
        udp::{create_datagram, UdpHeader},
    },
    socket::{handle_tcp_packet, handle_udp_packet},
};

use super::{hardware::HardwareAddress, protocol::arp::ArpPacket};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetEvent {
    LinkEstablished,

    ArpResponse(Ipv4Address),

    DhcpOffer(u32),
    DhcpAck(u32),

    DnsResponse,
}

pub enum NetRequest {
    GetLocalIp,
}

pub struct NetDevice {
    /// The MAC address of the network device
    pub mac: HardwareAddress,
    /// Holds an open handle to the network device driver.
    device_driver_handle: Handle,
    /// The wake set used to wake up the networking resident task when a network
    /// io operation completes.
    wake_set: Handle,
    /// Marks if the network device has already been opened. If false, the
    /// `active_read` op is actually the open operation.
    pub is_open: bool,
    /// A network device is always awaiting the next read. When a result arrives
    /// it wakes up the networking resident, which will check all devices for
    /// updates.
    active_read: Box<AsyncOp>,
    /// The buffer used for the current read operation.
    read_buffer: Vec<u8>,
    /// Writes are held until they complete, to ensure the payload and
    /// completion signal remain on the heap.
    active_writes: VecDeque<(Vec<u8>, Box<AsyncOp>)>,
    /// Established IP->MAC mappings
    pub known_arp: BTreeMap<Ipv4Address, HardwareAddress>,
    /// Stores DHCP info for the device
    pub dhcp_state: DhcpState,
}

impl NetDevice {
    pub fn new(device_path: &str, mac: HardwareAddress, wake_set: Handle) -> Self {
        let device_driver_handle = create_file_handle();

        let mut read_buffer = Vec::with_capacity(1536);
        for _ in 0..read_buffer.capacity() {
            read_buffer.push(0);
        }

        let active_read = Box::new(AsyncOp::new(
            ASYNC_OP_OPEN,
            device_path.as_ptr() as u32,
            device_path.len() as u32,
            0,
        ));
        let _ = send_io_op(device_driver_handle, &active_read, Some(wake_set));

        NetDevice {
            mac,
            device_driver_handle,
            wake_set,
            is_open: false,
            active_read,
            read_buffer,
            active_writes: VecDeque::new(),
            known_arp: BTreeMap::new(),
            dhcp_state: DhcpState::new(),
        }
    }

    /// Send a raw payload with an accompanying ethernet header.
    pub fn send_raw(
        &self,
        eth_header: EthernetFrameHeader,
        payload: &[u8],
    ) -> (Vec<u8>, Box<AsyncOp>) {
        let mut total_frame = Vec::with_capacity(EthernetFrameHeader::get_size() + payload.len());
        total_frame.extend_from_slice(eth_header.as_u8_buffer());
        total_frame.extend(payload);

        let async_op = Box::new(AsyncOp::new(
            ASYNC_OP_WRITE,
            total_frame.as_ptr() as u32,
            total_frame.len() as u32,
            0,
        ));
        let _ = send_io_op(self.device_driver_handle, &async_op, Some(self.wake_set));

        // return the vec so it can be stored, and not immediately dropped
        (total_frame, async_op)
    }

    /// The NetDevice holds onto async operations that are in progress.
    /// There may be multiple outstanding writes at the same time. Every time
    /// the device is awakened, it will clean up any writes that have completed.
    pub fn clear_completed_writes(&mut self) {
        loop {
            let can_pop = if let Some((_, pending_write)) = self.active_writes.front() {
                pending_write.is_complete()
            } else {
                false
            };
            if can_pop {
                let front = self.active_writes.pop_front();
                if let Some((_payload, op)) = front {
                    let return_value = op.return_value.load(Ordering::SeqCst);
                    // TODO: Check for errors?
                }
            } else {
                break;
            }
        }
    }

    pub fn add_write(&mut self, write: (Vec<u8>, Box<AsyncOp>)) {
        self.active_writes.push_back(write);
    }

    fn add_new_read_request(&mut self) {
        self.active_read = Box::new(AsyncOp::new(
            ASYNC_OP_READ,
            self.read_buffer.as_ptr() as u32,
            self.read_buffer.len() as u32,
            0,
        ));
        let _ = send_io_op(
            self.device_driver_handle,
            &self.active_read,
            Some(self.wake_set),
        );
    }

    pub fn process_read_result(&mut self) -> Option<NetEvent> {
        if !self.active_read.is_complete() {
            return None;
        }

        if !self.is_open {
            let result = self.active_read.return_value.load(Ordering::SeqCst);
            if result & 0x80000000 != 0 {
                // if opening the device failed, we try again? or destroy this
                // net device?
                super::resident::LOGGER.log(format_args!("Failed to open network device"));
                return None;
            }
            self.is_open = true;
            // we successfully opened the device, so we can now start reading
            self.add_new_read_request();
            return Some(NetEvent::LinkEstablished);
        }

        let result = self.active_read.return_value.load(Ordering::SeqCst);
        if result & 0x80000000 != 0 {
            // read failed
            super::resident::LOGGER.log(format_args!("Read failed"));
            self.add_new_read_request();
            return None;
        }
        let len = (result & 0x7fffffff) as usize;
        if len == 0 {
            // no data was read, so we can just continue
            super::resident::LOGGER.log(format_args!("No packet data"));
            self.add_new_read_request();
            return None;
        }

        // if data was successfully read, interpret the packet
        let event = if let Some(frame) = EthernetFrameHeader::try_from_u8_buffer(&self.read_buffer)
        {
            let offset = EthernetFrameHeader::get_size();
            match frame.get_ethertype() {
                // if it's an ARP response, process it with the device's ARP
                // state
                EthernetFrameHeader::ETHERTYPE_ARP => self.handle_arp_packet(offset),
                // if it's an IP packet, it may be UDP or TCP and needs to
                // be handled by the appropriate socket
                EthernetFrameHeader::ETHERTYPE_IP => self.handle_ip_packet(frame.src_mac, offset),
                _ => {
                    super::resident::LOGGER.log(format_args!("Unexpected ETHERTYPE"));
                    None
                }
            }
        } else {
            super::resident::LOGGER.log(format_args!("Invalid ethernet frame"));
            None
        };

        // zero out the read buffer and wait for another successful read
        for i in 0..self.read_buffer.len() {
            self.read_buffer[i] = 0;
        }
        self.add_new_read_request();

        event
    }

    /// An ARP packet may be a request, a broadcast, or a response to a request
    /// sent by this device. If it contains useful data, add that to the
    /// device's ARP state, and then wake any async tasks that might be blocked.
    fn handle_arp_packet(&mut self, offset: usize) -> Option<NetEvent> {
        let arp = ArpPacket::try_from_u8_buffer(&self.read_buffer[offset..]).unwrap();
        if arp.is_request() {
            // TODO
            return None;
        } else {
            // if it's a response, we can add the mapping to our ARP state
            let src_ip = arp.source_protocol_addr;
            let src_mac = arp.source_hardware_addr;
            self.known_arp.insert(src_ip, src_mac);
            // wake up any tasks that were waiting for this IP address
            return Some(NetEvent::ArpResponse(src_ip));
        }
    }

    fn handle_ip_packet(&mut self, _src_mac: HardwareAddress, offset: usize) -> Option<NetEvent> {
        let ip_header = Ipv4Header::try_from_u8_buffer(&self.read_buffer[offset..]).unwrap();
        let payload_offset = offset + Ipv4Header::get_size();
        let payload_length = u16::from_be(ip_header.total_length) as usize - Ipv4Header::get_size();
        let payload = &self.read_buffer[payload_offset..(payload_offset + payload_length)];

        if ip_header.protocol == IpProtocolType::Udp {
            let udp_header = match UdpHeader::try_from_u8_buffer(payload) {
                Some(header) => header,
                None => return None,
            };
            let dest_port = u16::from_be(udp_header.dest_port);
            let udp_payload = &payload[UdpHeader::get_size()..];
            if dest_port == 68 {
                // this is a DHCP packet
                super::resident::LOGGER.log(format_args!("Received DHCP packet"));
                // process dhcp packet, update state
                match self.dhcp_state.process_packet(self.mac, udp_payload) {
                    Ok((response, event)) => {
                        if let Some(res) = response {
                            // send the response
                            self.send_dhcp_packet(res);
                        }
                        return Some(event);
                    }
                    Err(_) => {}
                }
            } else if dest_port == *get_dns_port() {
                super::resident::LOGGER.log(format_args!("Received DNS packet"));
                match handle_dns_packet(udp_payload) {
                    Ok(_) => {
                        return Some(NetEvent::DnsResponse);
                    }
                    Err(_) => {}
                }
            } else {
                // this packet is bound for a socket
                super::resident::LOGGER.log(format_args!("UDP BOUND FOR :{}", dest_port));
                handle_udp_packet(
                    dest_port,
                    ip_header.source,
                    udp_header.source_port,
                    udp_payload,
                );
            }
        } else if ip_header.protocol == IpProtocolType::Tcp {
            super::resident::LOGGER.log(format_args!("TCP PACKET"));
            let tcp_header = match TcpHeader::try_from_u8_buffer(payload) {
                Some(header) => header,
                None => return None,
            };
            let tcp_payload = &payload[TcpHeader::get_size()..];
            let source_port = u16::from_be(tcp_header.source_port);
            let dest_port = u16::from_be(tcp_header.dest_port);
            super::resident::LOGGER.log(format_args!(
                "TCP from {}:{}, bound for :{}",
                ip_header.source, source_port, dest_port
            ));
            handle_tcp_packet(
                ip_header.dest,
                dest_port,
                ip_header.source,
                tcp_header,
                tcp_payload,
            );
        } else if ip_header.protocol == IpProtocolType::Icmp {
            super::resident::LOGGER.log(format_args!("ICMP PACKET"));
        }
        None
    }

    pub fn init_dhcp(&mut self, xid: u32) {
        let discovery_packet = DhcpPacket::discover(self.mac, xid);
        self.dhcp_state.local_ip = IpResolution::Progress(xid);
        self.send_dhcp_packet(discovery_packet);
    }

    fn send_dhcp_packet(&mut self, payload: Vec<u8>) {
        let eth_header = EthernetFrameHeader::new_ipv4(self.mac, HardwareAddress::BROADCAST);
        let ip_packet =
            create_datagram(Ipv4Address([0; 4]), 68, Ipv4Address([255; 4]), 67, &payload);
        let write = self.send_raw(eth_header, &ip_packet);
        self.add_write(write);
    }
}
