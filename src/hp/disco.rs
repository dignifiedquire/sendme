//! Contains the discovery message types.
//!
//! A discovery message is:
//!
//! Header:
//!
//!	magic          [u8; 6]  // “TS💬” (0x54 53 f0 9f 92 ac)
//!	senderDiscoPub [u8; 32] // nacl public key
//!	nonce          [u8; 24]
//!
//! The recipient then decrypts the bytes following (the nacl secretbox)
//! and then the inner payload structure is:
//!
//!	messageType     u8  (the MessageType constants below)
//!	messageVersion  u8  (0 for now; but always ignore bytes at the end)
//!	message-payload &[u8]

use std::{
    fmt::Display,
    net::{IpAddr, SocketAddr},
};

use anyhow::{anyhow, ensure, Result};

use crate::hp::stun::to_canonical;

use super::key;

// TODO: custom magicn
/// The 6 byte header of all discovery messages.
const MAGIC: &str = "TS💬"; // 6 bytes: 0x54 53 f0 9f 92 ac
const MAGIC_LEN: usize = MAGIC.as_bytes().len();

/// The length of the nonces used by nacl secretboxes.
const NONCE_LEN: usize = 24;

/// Current Version.
const V0: u8 = 0;

const KEY_LEN: usize = 32;
const EP_LENGTH: usize = 16 + 2; // 16 byte IP address + 2 byte port
const TX_LEN: usize = 12;

// Sizes for the inner message structure.

/// Header: Type | Version
const HEADER_LEN: usize = 2;

const PING_LEN: usize = TX_LEN + key::node::PUBLIC_KEY_LENGTH;
const PONG_LEN: usize = TX_LEN + EP_LENGTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    Ping = 0x01,
    Pong = 0x02,
    CallMeMaybe = 0x03,
}

impl TryFrom<u8> for MessageType {
    type Error = u8;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0x01 => Ok(MessageType::Ping),
            0x02 => Ok(MessageType::Pong),
            0x03 => Ok(MessageType::CallMeMaybe),
            _ => Err(value),
        }
    }
}

/// Reports whether p looks like it's a packet containing an encrypted disco message.
pub fn looks_like_disco_wrapper(p: &[u8]) -> bool {
    if p.len() < MAGIC_LEN + KEY_LEN * NONCE_LEN {
        return false;
    }

    &p[..MAGIC_LEN] == MAGIC.as_bytes()
}

/// If `p` looks like a disco message it returns the slice of `p` that represents the disco public key source.
pub fn source(p: &[u8]) -> Option<&[u8]> {
    if !looks_like_disco_wrapper(p) {
        return None;
    }

    Some(&p[MAGIC_LEN..MAGIC_LEN + KEY_LEN])
}

/// A discovery message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Ping(Ping),
    Pong(Pong),
    CallMeMaybe(CallMeMaybe),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    /// Random client-generated per-ping transaction ID.
    pub tx_id: [u8; 12],

    /// Allegedly the ping sender's wireguard public key.
    /// It shouldn't be trusted by itself, but can be combined with
    /// netmap data to reduce the discokey:nodekey relation from 1:N to 1:1.
    pub node_key: key::node::PublicKey,
}

/// A response a Ping.
///
/// It includes the sender's source IP + port, so it's effectively a STUN response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    pub tx_id: [u8; 12],
    /// 18 bytes (16+2) on the wire; v4-mapped ipv6 for IPv4.
    pub src: SocketAddr,
}
/// Message sent only over DERP to request that the recipient try
/// to open up a magicsock path back to the sender.
///
/// The sender should've already sent UDP packets to the peer to open
/// up the stateful firewall mappings inbound.
///
/// The recipient may choose to not open a path back, if it's already happy with its path.
/// But usually it will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMeMaybe {
    /// What the peer believes its endpoints are.
    pub my_number: Vec<SocketAddr>,
}

impl Ping {
    fn from_bytes(ver: u8, p: &[u8]) -> Result<Self> {
        ensure!(ver == V0, "invalid version");
        // Deliberately lax on longer-than-expected messages, for future compatibility.
        ensure!(p.len() >= PING_LEN, "message too short");
        let tx_id: [u8; TX_LEN] = p[..TX_LEN].try_into().unwrap();
        let raw_key: [u8; key::node::PUBLIC_KEY_LENGTH] = p
            [TX_LEN..TX_LEN + key::node::PUBLIC_KEY_LENGTH]
            .try_into()
            .unwrap();
        let node_key = key::node::PublicKey::from(raw_key);

        Ok(Ping { tx_id, node_key })
    }

    fn as_bytes(&self) -> Vec<u8> {
        let header = msg_header(MessageType::Ping, V0);
        let mut out = vec![0u8; PING_LEN + HEADER_LEN];

        out[..HEADER_LEN].copy_from_slice(&header);
        out[HEADER_LEN..HEADER_LEN + TX_LEN].copy_from_slice(&self.tx_id);
        out[HEADER_LEN + TX_LEN..].copy_from_slice(self.node_key.as_ref());

        out
    }
}

// Assumes p.len() == EP_LENGTH
fn socket_addr_from_bytes(p: &[u8]) -> SocketAddr {
    debug_assert_eq!(p.len(), EP_LENGTH);

    let raw_src_ip: [u8; 16] = p[..16].try_into().unwrap();
    let raw_port: [u8; 2] = p[16..].try_into().unwrap();

    let src_ip = to_canonical(IpAddr::from(raw_src_ip));
    let src_port = u16::from_le_bytes(raw_port);
    let src = SocketAddr::new(src_ip, src_port);

    src
}

fn socket_addr_as_bytes(addr: &SocketAddr) -> [u8; EP_LENGTH] {
    let mut out = [0u8; EP_LENGTH];
    let ipv6 = match addr.ip() {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    };
    out[..16].copy_from_slice(&ipv6.octets());
    out[16..].copy_from_slice(&addr.port().to_le_bytes());

    out
}

impl Pong {
    fn from_bytes(ver: u8, p: &[u8]) -> Result<Self> {
        ensure!(ver == V0, "invalid version");
        ensure!(p.len() >= PONG_LEN, "message too short");
        let tx_id: [u8; TX_LEN] = p[..TX_LEN].try_into().unwrap();

        let src = socket_addr_from_bytes(&p[TX_LEN..TX_LEN + EP_LENGTH]);

        Ok(Pong { tx_id, src })
    }

    fn as_bytes(&self) -> Vec<u8> {
        let header = msg_header(MessageType::Pong, V0);
        let mut out = vec![0u8; PONG_LEN + HEADER_LEN];

        out[..HEADER_LEN].copy_from_slice(&header);
        out[HEADER_LEN..HEADER_LEN + TX_LEN].copy_from_slice(&self.tx_id);

        let src_bytes = socket_addr_as_bytes(&self.src);
        out[HEADER_LEN + TX_LEN..].copy_from_slice(&src_bytes);
        out
    }
}

impl CallMeMaybe {
    fn from_bytes(ver: u8, p: &[u8]) -> Result<Self> {
        ensure!(ver == V0, "invalid version");
        ensure!(p.len() % EP_LENGTH == 0, "invalid entries");

        let num_entries = p.len() / EP_LENGTH;
        let mut m = CallMeMaybe {
            my_number: Vec::with_capacity(num_entries),
        };

        for chunk in p.chunks_exact(EP_LENGTH) {
            let src = socket_addr_from_bytes(chunk);
            m.my_number.push(src);
        }

        Ok(m)
    }

    fn as_bytes(&self) -> Vec<u8> {
        let header = msg_header(MessageType::CallMeMaybe, V0);
        let mut out = vec![0u8; HEADER_LEN + self.my_number.len() * EP_LENGTH];
        out[..HEADER_LEN].copy_from_slice(&header);

        for (m, chunk) in self
            .my_number
            .iter()
            .zip(out[HEADER_LEN..].chunks_exact_mut(EP_LENGTH))
        {
            let raw = socket_addr_as_bytes(m);
            chunk.copy_from_slice(&raw);
        }

        out
    }
}

impl Message {
    /// Parses the encrypted part of the message from inside the nacl secretbox.
    pub fn from_bytes(p: &[u8]) -> Result<Self> {
        ensure!(p.len() >= 2, "message too short");

        let t = MessageType::try_from(p[0]).map_err(|v| anyhow!("unkown message type: {}", v))?;
        let ver = p[1];
        let p = &p[2..];
        match t {
            MessageType::Ping => {
                let ping = Ping::from_bytes(ver, p)?;
                Ok(Message::Ping(ping))
            }
            MessageType::Pong => {
                let pong = Pong::from_bytes(ver, p)?;
                Ok(Message::Pong(pong))
            }
            MessageType::CallMeMaybe => {
                let cm = CallMeMaybe::from_bytes(ver, p)?;
                Ok(Message::CallMeMaybe(cm))
            }
        }
    }

    /// Serialize this message to bytes.
    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            Message::Ping(ping) => ping.as_bytes(),
            Message::Pong(pong) => pong.as_bytes(),
            Message::CallMeMaybe(cm) => cm.as_bytes(),
        }
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Ping(ping) => {
                write!(f, "ping tx={:?}", &ping.tx_id[..6])
            }
            Message::Pong(pong) => {
                write!(f, "ping tx={:?}", &pong.tx_id[..6])
            }
            Message::CallMeMaybe(_) => {
                write!(f, "call-me-maybe")
            }
        }
    }
}

const fn msg_header(t: MessageType, ver: u8) -> [u8; HEADER_LEN] {
    [t as u8, ver]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_from_bytes() {
        struct Test {
            name: &'static str,
            m: Message,
            want: &'static str,
        }
        let tests = [
	    Test {
		name: "ping_with_nodekey_src",
		m: Message::Ping(Ping {
		    tx_id:    [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
		    node_key: key::node::PublicKey::from([0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 30, 31]),
		}),
		want: "01 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 00 01 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 1e 1f",
	    },
	    Test {
		name: "pong",
		m: Message::Pong(Pong{
		    tx_id: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
		    src:  "2.3.4.5:1234".parse().unwrap(),
		}),
		want: "02 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 00 00 00 00 00 00 00 00 00 00 ff ff 02 03 04 05 d2 04",
	    },
	    Test {
		name: "pongv6",
		m: Message::Pong(Pong {
		    tx_id: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
		    src:  "[fed0::12]:6666".parse().unwrap(),
		}),
		want: "02 00 01 02 03 04 05 06 07 08 09 0a 0b 0c fe d0 00 00 00 00 00 00 00 00 00 00 00 00 00 12 0a 1a",
	    },
	    Test {
		name: "call_me_maybe",
		m:    Message::CallMeMaybe(CallMeMaybe { my_number: Vec::new() }),
		want: "03 00",
	    },
	    Test {
		name: "call_me_maybe_endpoints",
		m: Message::CallMeMaybe(CallMeMaybe {
		    my_number: vec![
			"1.2.3.4:567".parse().unwrap(),
			"[2001::3456]:789".parse().unwrap(),
		    ],
		}),
		want: "03 00 00 00 00 00 00 00 00 00 00 00 ff ff 01 02 03 04 37 02 20 01 00 00 00 00 00 00 00 00 00 00 00 00 34 56 15 03",
	    },
	];
        for test in tests {
            println!("{}", test.name);

            let got = test.m.as_bytes();
            assert_eq!(
                got,
                hex::decode(test.want.replace(" ", "")).unwrap(),
                "wrong as_bytes"
            );

            let back = Message::from_bytes(&got).expect("failed to parse");
            assert_eq!(test.m, back, "wrong from_bytes");
        }
    }
}
