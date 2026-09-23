//! Minimal STUN Binding client (RFC 5389) to learn the public address of our UDP socket, which
//! str0m then advertises as a server-reflexive candidate. str0m does no candidate gathering.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rand::RngExt;
use tokio::net::UdpSocket;

const MAGIC: u32 = 0x2112_A442;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub async fn server_reflexive(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let server = tokio::net::lookup_host(server)
        .await?
        .find(|a| a.is_ipv4() == socket.local_addr().map(|l| l.is_ipv4()).unwrap_or(true))
        .ok_or_else(|| anyhow!("cannot resolve STUN server"))?;
    let txid: [u8; 12] = rand::rng().random();
    let mut req = Vec::with_capacity(20);
    req.extend_from_slice(&0x0001u16.to_be_bytes()); // Binding request
    req.extend_from_slice(&0u16.to_be_bytes());
    req.extend_from_slice(&MAGIC.to_be_bytes());
    req.extend_from_slice(&txid);

    let mut buf = [0u8; 512];
    for attempt in 0..3 {
        socket.send_to(&req, server).await?;
        let wait = Duration::from_millis(400 << attempt);
        let Ok(res) = tokio::time::timeout(wait, socket.recv_from(&mut buf)).await else {
            continue;
        };
        let (n, from) = res?;
        if from == server
            && let Ok(addr) = parse_response(&buf[..n], &txid)
        {
            return Ok(addr);
        }
    }
    bail!("no STUN response from {server}")
}

fn parse_response(msg: &[u8], txid: &[u8; 12]) -> Result<SocketAddr> {
    if msg.len() < 20
        || msg[0..2] != [0x01, 0x01]
        || msg[4..8] != MAGIC.to_be_bytes()
        || &msg[8..20] != txid
    {
        bail!("not our binding success response");
    }
    let mut attrs = &msg[20..];
    while attrs.len() >= 4 {
        let kind = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let value = attrs
            .get(4..4 + len)
            .ok_or_else(|| anyhow!("truncated attribute"))?;
        if kind == XOR_MAPPED_ADDRESS && len >= 8 {
            let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC >> 16) as u16;
            let ip = match value[1] {
                0x01 => {
                    let raw = u32::from_be_bytes([value[4], value[5], value[6], value[7]]) ^ MAGIC;
                    IpAddr::V4(Ipv4Addr::from(raw))
                }
                0x02 if len >= 20 => {
                    let mut key = [0u8; 16];
                    key[..4].copy_from_slice(&MAGIC.to_be_bytes());
                    key[4..].copy_from_slice(txid);
                    let mut raw = [0u8; 16];
                    for i in 0..16 {
                        raw[i] = value[4 + i] ^ key[i];
                    }
                    IpAddr::V6(Ipv6Addr::from(raw))
                }
                _ => bail!("unknown address family"),
            };
            return Ok(SocketAddr::new(ip, port));
        }
        attrs = &attrs[(4 + len + 3) & !3..];
    }
    bail!("no XOR-MAPPED-ADDRESS")
}

/// The LAN address the OS would use to reach the internet (no packets are sent).
pub fn primary_local_ip() -> Result<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
    s.connect("1.1.1.1:53")?;
    Ok(s.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc5769_ipv4_vector() {
        // RFC 5769 §2.2 sample response (XOR-MAPPED-ADDRESS 192.0.2.1:32853).
        let txid = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut msg = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&txid);
        msg.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        assert_eq!(
            parse_response(&msg, &txid).unwrap(),
            "192.0.2.1:32853".parse().unwrap()
        );
        assert!(parse_response(&msg, &[0; 12]).is_err());
    }
}
