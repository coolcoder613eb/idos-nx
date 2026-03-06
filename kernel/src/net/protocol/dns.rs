use core::sync::atomic::{AtomicU16, Ordering};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use spin::RwLock;

use crate::net::socket::{get_ephemeral_port, port::SocketPort};

use super::{ipv4::Ipv4Address, packet::PacketHeader};

#[repr(C, packed)]
pub struct DnsHeader {
    pub id: u16,
    pub flags: u16,
    pub question_count: u16,
    pub answer_count: u16,
    pub authority_count: u16,
    pub additional_count: u16,
}

impl DnsHeader {
    pub const FLAG_RESPONSE: u16 = 0x8000; // Query/Response
    pub const FLAG_AA: u16 = 0x0400; // Authoritative answer
    pub const FLAG_TC: u16 = 0x0200; // Truncated response
    pub const FLAG_RD: u16 = 0x0100; // Recursion desired
    pub const FLAG_RA: u16 = 0x0080; // Recursion available

    pub fn is_response(&self) -> bool {
        self.flags & Self::FLAG_RESPONSE != 0
    }

    pub fn build_query_header(id: u16, question_count: u16) -> Self {
        let flags = Self::FLAG_RD;
        Self {
            id: id.to_be(),
            flags: flags.to_be(),
            question_count: question_count.to_be(),
            answer_count: 0,
            authority_count: 0,
            additional_count: 0,
        }
    }

    pub fn build_query_packet(questions: &[DnsQuestion]) -> Vec<u8> {
        let expected_size =
            Self::get_size() + questions.iter().map(|q| q.name_length() + 4).sum::<usize>(); // include 4 bytes per question for type and class
        let mut packet = Vec::with_capacity(expected_size);
        let mut xid_bytes: [u8; 2] = [0; 2];
        crate::random::get_random_bytes(&mut xid_bytes);
        let xid: u16 = u16::from_le_bytes(xid_bytes);
        let header = Self::build_query_header(xid, questions.len() as u16);
        packet.extend_from_slice(header.as_u8_buffer());

        for question in questions {
            match question {
                DnsQuestion::A(name) => {
                    packet.extend_from_slice(name);
                    packet.extend_from_slice(&[0, 1]); // A record
                    packet.extend_from_slice(&[0, 1]); // Class IN
                }
                DnsQuestion::Cname(name) => {
                    packet.extend_from_slice(name);
                    packet.extend_from_slice(&[0, 5]); // CNAME record
                    packet.extend_from_slice(&[0, 1]);
                }
            }
        }

        packet
    }
}

impl PacketHeader for DnsHeader {}

// it is expected that all names are already null-terminated
pub enum DnsQuestion {
    A(Vec<u8>),
    Cname(Vec<u8>),
}

impl DnsQuestion {
    pub fn name_length(&self) -> usize {
        match self {
            DnsQuestion::A(name) | DnsQuestion::Cname(name) => name.len() + 1, // extra byte for null terminator
        }
    }

    pub fn a_record(name: String) -> Self {
        // convert the domain name to DNS format
        let expected_length = name.len() + 2; // account for first label length and null terminator
        let mut encoded_name = Vec::with_capacity(expected_length);
        let labels = name.split('.');
        for label in labels {
            encoded_name.push(label.len() as u8);
            encoded_name.extend_from_slice(label.as_bytes());
        }
        encoded_name.push(0);
        DnsQuestion::A(encoded_name)
    }
}

static DNS_PORT: AtomicU16 = AtomicU16::new(0);

static DNS_CACHE: RwLock<BTreeMap<String, Ipv4Address>> = RwLock::new(BTreeMap::new());

pub fn get_dns_port() -> SocketPort {
    let load = DNS_PORT.load(Ordering::SeqCst);
    if load == 0 {
        let new_port = get_ephemeral_port().unwrap();

        DNS_PORT.store(*new_port, Ordering::SeqCst);
        new_port
    } else {
        SocketPort::new(load)
    }
}

pub fn lookup_dns(name: &str) -> Option<Ipv4Address> {
    let cache = DNS_CACHE.read();
    cache.get(name).cloned()
}

pub fn add_dns_cache(name: String, address: Ipv4Address) {
    let mut cache = DNS_CACHE.write();
    cache.insert(name, address);
}

pub fn handle_dns_packet(packet: &[u8]) -> Result<(), ()> {
    let header = DnsHeader::try_from_u8_buffer(packet).ok_or(())?;
    if !header.is_response() {
        return Err(());
    }
    if header.answer_count == 0 {
        return Err(());
    }

    let mut parser = DnsParser::new(&packet[DnsHeader::get_size()..]);
    let question_count = u16::from_be(header.question_count);
    let answer_count = u16::from_be(header.answer_count);
    for _ in 0..question_count {
        let _ = parser.parse_question();
    }
    for _ in 0..answer_count {
        let record = match parser.parse_answer() {
            Ok(r) => r,
            Err(_) => continue,
        };
        match record.record_data {
            RecordData::A(ip) => {
                crate::kprintln!("DNS: {} is at {}", record.name, ip);
                add_dns_cache(record.name, ip);
            }
            RecordData::Cname(cname) => {}
        }
    }

    Ok(())
}

enum RecordData {
    A(Ipv4Address),
    Cname(String),
}

struct DnsRecord {
    name: String,
    record_type: u16,
    class: u16,
    ttl: u32,
    record_data: RecordData,
}

struct DnsParser<'a> {
    packet: &'a [u8],
    pos: usize,
}

impl<'a> DnsParser<'a> {
    pub fn new(packet: &'a [u8]) -> Self {
        Self { packet, pos: 0 }
    }

    pub fn read_u8(&mut self) -> Result<u8, ()> {
        if self.pos >= self.packet.len() {
            return Err(());
        }
        let value = self.packet[self.pos];
        self.pos += 1;
        Ok(value)
    }

    pub fn read_u16(&mut self) -> Result<u16, ()> {
        if self.pos + 1 >= self.packet.len() {
            return Err(());
        }
        let value = u16::from_be_bytes([self.packet[self.pos], self.packet[self.pos + 1]]);
        self.pos += 2;
        Ok(value)
    }

    pub fn read_u32(&mut self) -> Result<u32, ()> {
        if self.pos + 3 >= self.packet.len() {
            return Err(());
        }
        let value = u32::from_be_bytes([
            self.packet[self.pos],
            self.packet[self.pos + 1],
            self.packet[self.pos + 2],
            self.packet[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(value)
    }

    pub fn read_n_bytes(&mut self, n: usize) -> Result<&[u8], ()> {
        if self.pos + n > self.packet.len() {
            return Err(());
        }
        let bytes = &self.packet[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    pub fn parse_name(&mut self) -> Result<String, ()> {
        let mut name_parts = Vec::new();
        let mut jumped = false;
        // store the position if we jump to a compression pointer
        let mut original_pos = self.pos;

        loop {
            let label_len = self.read_u8()?;

            if label_len == 0 {
                break;
            }

            if label_len & 0xc0 == 0xc0 {
                // Compression pointer
                if !jumped {
                    original_pos = self.pos + 1; // Save position after pointer
                    jumped = true;
                }

                let pointer = ((((label_len & 0x3f) as u16) << 8) | self.read_u8()? as u16)
                    - (DnsHeader::get_size() as u16);
                self.pos = pointer as usize;

                if self.pos >= self.packet.len() {
                    return Err(());
                }
                continue;
            }

            if label_len & 0xc0 != 0 {
                return Err(());
            }

            let label_bytes = self.read_n_bytes(label_len as usize)?;
            let label = String::from_utf8_lossy(label_bytes).to_string();
            name_parts.push(label);
        }

        if jumped {
            self.pos = original_pos;
        }

        Ok(name_parts.join("."))
    }

    pub fn parse_question(&mut self) -> Result<DnsQuestion, ()> {
        let name = self.parse_name()?;
        let record_type = self.read_u16()?;
        let class = self.read_u16()?;

        if class != 1 {
            // Class IN
            return Err(());
        }

        match record_type {
            1 => Ok(DnsQuestion::A(name.into_bytes())),
            5 => Ok(DnsQuestion::Cname(name.into_bytes())),
            _ => Err(()), // Unsupported record type
        }
    }

    pub fn parse_answer(&mut self) -> Result<DnsRecord, ()> {
        let name = self.parse_name()?;
        let record_type = self.read_u16()?;
        let class = self.read_u16()?;
        let ttl = self.read_u32()?;
        let data_length = self.read_u16()? as usize;

        if class != 1 {
            // Class IN
            return Err(());
        }

        if data_length == 0 {
            return Err(()); // No data
        }

        let record_data = match record_type {
            1 => {
                // A record
                if data_length != 4 {
                    return Err(()); // Invalid length for A record
                }
                RecordData::A(Ipv4Address([
                    self.read_u8()?,
                    self.read_u8()?,
                    self.read_u8()?,
                    self.read_u8()?,
                ]))
            }
            5 => {
                // CNAME record
                let cname = self.parse_name()?;
                RecordData::Cname(cname)
            }
            _ => return Err(()), // Unsupported record type
        };

        Ok(DnsRecord {
            name,
            record_type,
            class,
            ttl,
            record_data,
        })
    }
}
