use crate::{
    collections::SlotList,
    executor::{Executor, WaitForEvent, WakerRegistry},
    io::handle::Handle,
    log::TaggedLogger,
    sync::wake_set::WakeSet,
    task::actions::{
        io::write_sync,
        sync::{block_on_wake_set, create_wake_set, get_inner_wake_set},
    },
};

use super::{
    hardware::HardwareAddress,
    netdevice::{NetDevice, NetEvent},
    protocol::{
        arp::ArpPacket,
        dhcp::IpResolution,
        dns::{get_dns_port, lookup_dns, DnsHeader, DnsQuestion},
        ethernet::EthernetFrameHeader,
        ipv4::Ipv4Address,
        packet::PacketHeader,
        tcp::header::TcpHeader,
        udp::create_datagram,
    },
    socket::{create_dns_socket, port::SocketPort},
};
use alloc::{collections::VecDeque, string::String, sync::Arc, vec::Vec};
use core::str::FromStr;

use spin::{Mutex, RwLock};

pub const LOGGER: TaggedLogger = TaggedLogger::new("NET", 92);

pub enum NetRequest {
    RegisterDevice(String, HardwareAddress),
    GetIp,
    GetMacForIp(Ipv4Address),
    OpenTcp(SocketPort, String, SocketPort),
    Respond(Ipv4Address, Vec<u8>),
}

static NET_STACK_REQUESTS: Mutex<VecDeque<NetRequest>> = Mutex::new(VecDeque::new());
static WAKE_SET: RwLock<Option<Arc<WakeSet>>> = RwLock::new(None);

pub fn register_network_device(name: &str, mac: [u8; 6]) {
    NET_STACK_REQUESTS
        .lock()
        .push_back(NetRequest::RegisterDevice(
            String::from(name),
            HardwareAddress(mac),
        ));

    let wake_set = WAKE_SET.read().clone();
    if let Some(waker) = wake_set {
        waker.wake();
    }
}

pub fn get_ip() {
    NET_STACK_REQUESTS.lock().push_back(NetRequest::GetIp);

    let wake_set = WAKE_SET.read().clone();
    if let Some(waker) = wake_set {
        waker.wake();
    }
}

pub fn get_mac_for_ip(ip: Ipv4Address) {
    NET_STACK_REQUESTS
        .lock()
        .push_back(NetRequest::GetMacForIp(ip));

    let wake_set = WAKE_SET.read().clone();
    if let Some(waker) = wake_set {
        waker.wake();
    }
}

pub fn net_open_tcp(local_port: SocketPort, destination: String, remote_port: SocketPort) {
    NET_STACK_REQUESTS
        .lock()
        .push_back(NetRequest::OpenTcp(local_port, destination, remote_port));

    let wake_set = WAKE_SET.read().clone();
    if let Some(waker) = wake_set {
        waker.wake();
    }
}

/// Special case of net_send where the destionation IP is already available
/// because we are responding to an incoming packet.
pub fn net_respond(destination: Ipv4Address, payload: Vec<u8>) {
    NET_STACK_REQUESTS
        .lock()
        .push_back(NetRequest::Respond(destination, payload));

    let wake_set = WAKE_SET.read().clone();
    if let Some(waker) = wake_set {
        waker.wake();
    }
}

pub fn net_stack_resident() -> ! {
    // this wake set will be used to listen for all network devices
    // each time a new network device is registered, it will be passed in and
    // the io operations will notify it
    let wake_set = create_wake_set();
    WAKE_SET
        .write()
        .replace(get_inner_wake_set(wake_set).unwrap());

    let mut network_devices: SlotList<(Executor<NetEvent>, Arc<RwLock<NetDevice>>)> =
        SlotList::new();

    // let the init task know that the network stack is ready
    let response_writer = Handle::new(0);
    let _ = write_sync(response_writer, &[1], 0);

    // each time a device is registered, open it and add its handle to the
    // notify queue
    // also add a pending read op, which can be read from within the loop
    //
    // each network device also has an associated state machine which stores
    // its own ARP, DHCP, and socket states

    let dns_port = get_dns_port();
    create_dns_socket(dns_port);

    loop {
        // check the task queue for external requests
        // External async requests include:
        //  - Register network device by name + MAC
        //  - Socket accept / send / receive / close
        //  - IP lookup (async because DHCP may not have been established yet)
        if let Some(mut queue) = NET_STACK_REQUESTS.try_lock() {
            while let Some(req) = queue.pop_front() {
                match req {
                    NetRequest::RegisterDevice(name, mac) => {
                        LOGGER.log(format_args!("Register Device {}", name));
                        let mut executor = Executor::<NetEvent>::new();
                        let netdev = Arc::new(RwLock::new(NetDevice::new(&name, mac, wake_set)));
                        let dev_clone = netdev.clone();
                        let waker_reg = executor.waker_registry();
                        executor.spawn(async move {
                            let _ = get_local_ip(dev_clone, waker_reg).await;
                        });
                        network_devices.insert((executor, netdev));
                    }
                    NetRequest::GetIp => {
                        let (executor, active_device) = network_devices.get_mut(0).unwrap();
                        let device = active_device.clone();
                        let waker_reg = executor.waker_registry();
                        executor.spawn(async move {
                            let _ = get_local_ip(device, waker_reg).await;
                        });
                    }
                    NetRequest::GetMacForIp(ip) => {
                        let (executor, active_device) = network_devices.get_mut(0).unwrap();
                        let device = active_device.clone();
                        let waker_reg = executor.waker_registry();
                        executor.spawn(async move {
                            if let Some(mac) = resolve_ip_to_mac(ip, device, waker_reg).await {
                                LOGGER.log(format_args!("Resolved {} to MAC {}", ip, mac));
                            } else {
                                LOGGER.log(format_args!("Failed to resolve IP {}", ip));
                            }
                        });
                    }
                    NetRequest::OpenTcp(local_port, dest, remote_port) => {
                        LOGGER.log(format_args!(
                            "OPEN TCP {} -> {}{}",
                            local_port, dest, remote_port
                        ));
                        let (executor, active_device) = network_devices.get_mut(0).unwrap();
                        let device = active_device.clone();
                        let waker_reg = executor.waker_registry();
                        executor.spawn(async move {
                            match open_tcp(local_port, dest, remote_port, device, waker_reg).await {
                                Ok(_) => {}
                                Err(_) => {
                                    LOGGER.log(format_args!("Failed to open TCP connection"));
                                }
                            }
                        });
                    }
                    NetRequest::Respond(dest_ip, payload) => {
                        LOGGER.log(format_args!("RESPOND TO {}", dest_ip));
                        let (executor, active_device) = network_devices.get_mut(0).unwrap();
                        let device = active_device.clone();
                        let waker_reg = executor.waker_registry();
                        executor.spawn(async move {
                            match send_packet_direct(dest_ip, payload, device, waker_reg).await {
                                Ok(_) => {}
                                Err(_) => {
                                    LOGGER.log(format_args!("Failed to respond to packet"));
                                }
                            }
                        });
                    }
                }
            }
        }

        // For each device, check the read op
        for (executor, net_dev_lock) in network_devices.iter_mut() {
            let read_event = {
                let mut net_dev = net_dev_lock.write();
                net_dev.clear_completed_writes();
                net_dev.process_read_result()
            };
            if let Some(event) = read_event {
                executor.notify_event(&event);
            }
            executor.poll_tasks();
        }

        block_on_wake_set(wake_set, None);
    }
}

async fn get_local_ip(
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Option<Ipv4Address> {
    let is_open = net_dev_lock.read().is_open;
    if !is_open {
        WaitForEvent::new(NetEvent::LinkEstablished, waker_registry.clone()).await;
    }
    let resolved_ip = net_dev_lock.read().dhcp_state.local_ip.clone();
    let xid = match resolved_ip {
        IpResolution::Bound(ip, _expiration) => {
            return Some(ip);
        }
        IpResolution::Unbound => {
            // never initialized before, run the whole process
            LOGGER.log(format_args!("INIT DHCP"));
            let mut xid_bytes: [u8; 4] = [0; 4];
            crate::random::get_random_bytes(&mut xid_bytes);
            let xid: u32 = u32::from_le_bytes(xid_bytes);
            net_dev_lock.write().init_dhcp(xid);
            // after sending the broadcast, wait for an offer
            WaitForEvent::new(NetEvent::DhcpOffer(xid), waker_registry.clone()).await;
            xid
        }
        IpResolution::Progress(xid) => xid,
        IpResolution::Renewing(_, xid) => xid,
    };
    WaitForEvent::new(NetEvent::DhcpAck(xid), waker_registry.clone()).await;

    let final_state = net_dev_lock.read().dhcp_state.local_ip.clone();
    match final_state {
        IpResolution::Bound(ip, _expiration) => {
            super::socket::listen::flush_pending_udp_writes(ip);
            Some(ip)
        }
        _ => None,
    }
}

async fn resolve_ip_to_mac(
    target_ip: Ipv4Address,
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Option<HardwareAddress> {
    let is_open = net_dev_lock.read().is_open;
    if !is_open {
        WaitForEvent::new(NetEvent::LinkEstablished, waker_registry.clone()).await;
    }
    if let Some(mac) = net_dev_lock.read().known_arp.get(&target_ip).cloned() {
        return Some(mac);
    }

    // If not known, send an ARP request and wait for a response
    let local_ip = get_local_ip(net_dev_lock.clone(), waker_registry.clone()).await?;
    {
        let mut net_dev = net_dev_lock.write();
        let local_mac = net_dev.mac;
        let arp_request = ArpPacket::request(local_mac, local_ip, target_ip);
        let eth_frame = EthernetFrameHeader::broadcast_arp(local_mac);
        let write = net_dev.send_raw(eth_frame, arp_request.as_u8_buffer());
        net_dev.add_write(write);
    }

    WaitForEvent::new(NetEvent::ArpResponse(target_ip), waker_registry).await;
    LOGGER.log(format_args!("Resolving IP {} to MAC", target_ip));

    net_dev_lock.read().known_arp.get(&target_ip).cloned()
}

async fn get_next_hop(
    destination: Ipv4Address,
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Option<HardwareAddress> {
    let local_ip = get_local_ip(net_dev_lock.clone(), waker_registry.clone()).await?;
    let net_mask = net_dev_lock.read().dhcp_state.subnet_mask;
    let local_masked = local_ip & net_mask;
    let dest_masked = destination & net_mask;
    if local_masked == dest_masked {
        // If the destination is on the same subnet, we can use ARP to resolve it
        resolve_ip_to_mac(destination, net_dev_lock, waker_registry).await
    } else {
        // If the destination is not on the same subnet, we need to use a gateway
        let gateway = net_dev_lock.read().dhcp_state.gateway_ip;
        resolve_ip_to_mac(gateway, net_dev_lock, waker_registry).await
    }
}

async fn open_tcp(
    local_port: SocketPort,
    destination: String,
    remote_port: SocketPort,
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Result<(), ()> {
    let local_addr = get_local_ip(net_dev_lock.clone(), waker_registry.clone())
        .await
        .ok_or(())?;
    let remote_addr = if let Ok(ip) = Ipv4Address::from_str(&destination) {
        ip
    } else {
        dns_lookup(destination, net_dev_lock.clone(), waker_registry.clone()).await?
    };
    let tcp_packet = TcpHeader::create_packet(
        local_addr,
        local_port,
        remote_addr,
        remote_port,
        0,                   // seq number
        0,                   // ack number
        TcpHeader::FLAG_SYN, // flags
        &[],
    );
    send_packet_direct(remote_addr, tcp_packet, net_dev_lock, waker_registry).await
}

async fn send_packet_direct(
    destination: Ipv4Address,
    payload: Vec<u8>,
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Result<(), ()> {
    let next_hop = match get_next_hop(destination, net_dev_lock.clone(), waker_registry).await {
        Some(mac) => mac,
        None => {
            LOGGER.log(format_args!("No route to {}", destination));
            return Err(());
        }
    };

    let eth_header = EthernetFrameHeader::new_ipv4(net_dev_lock.read().mac, next_hop);

    {
        let mut net_dev = net_dev_lock.write();
        let write = net_dev.send_raw(eth_header, &payload);
        net_dev.add_write(write);
    }
    Ok(())
}

async fn dns_lookup(
    hostname: String,
    net_dev_lock: Arc<RwLock<NetDevice>>,
    waker_registry: WakerRegistry<NetEvent>,
) -> Result<Ipv4Address, ()> {
    LOGGER.log(format_args!("DNS lookup for {}", hostname));
    // First, check if the hostname is already cached
    if let Some(ip) = lookup_dns(&hostname) {
        return Ok(ip);
    }
    let local_ip = get_local_ip(net_dev_lock.clone(), waker_registry.clone())
        .await
        .ok_or(())?;
    let dns_server_ip = net_dev_lock
        .read()
        .dhcp_state
        .dns_servers
        .get(0)
        .copied()
        .ok_or(())?;

    let dns_packet = DnsHeader::build_query_packet(&[DnsQuestion::a_record(hostname.clone())]);

    let outbound_port: u16 = *get_dns_port();
    let eth_header =
        EthernetFrameHeader::new_ipv4(net_dev_lock.read().mac, HardwareAddress::BROADCAST);
    let ip_packet = create_datagram(local_ip, outbound_port, dns_server_ip, 53, &dns_packet);
    {
        let mut net_dev = net_dev_lock.write();
        let write = net_dev.send_raw(eth_header, &ip_packet);
        net_dev.add_write(write);
    }

    loop {
        WaitForEvent::new(NetEvent::DnsResponse, waker_registry.clone()).await;
        if let Some(ip) = lookup_dns(&hostname) {
            return Ok(ip);
        }
    }

    Err(())
}
