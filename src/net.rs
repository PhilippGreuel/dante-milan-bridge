//! Raw-Ethernet-Zugriff über pcap (Npcap unter Windows, libpcap/BPF unter macOS).

use anyhow::{anyhow, Context};
use parking_lot::Mutex;
use pcap::{Active, Capture};
use std::sync::atomic::{AtomicU64, Ordering};

pub type Mac = [u8; 6];

pub const ETH_AVTP: u16 = 0x22F0;
pub const ETH_MSRP: u16 = 0x22EA;
pub const ETH_MVRP: u16 = 0x88F5;
pub const ETH_GPTP: u16 = 0x88F7;
pub const ETH_VLAN: u16 = 0x8100;

pub const MAC_GPTP_MSRP: Mac = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E];
pub const MAC_MVRP: Mac = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x21];
pub const MAC_AVDECC: Mac = [0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00];
pub const MAC_MAAP: Mac = [0x91, 0xE0, 0xF0, 0x00, 0xFF, 0x00];

#[derive(Clone, Debug)]
pub struct NetIf {
    pub pcap_name: String,
    pub label: String,
    pub mac: Option<Mac>,
}

pub fn list_interfaces() -> anyhow::Result<Vec<NetIf>> {
    let os_ifs = netdev::get_interfaces();
    let mut out = Vec::new();
    for d in pcap::Device::list().context("pcap: Geräteliste nicht lesbar (Npcap/libpcap installiert?)")? {
        // Windows: pcap-Name "\Device\NPF_{GUID}", netdev-Name "{GUID}"
        let os = os_ifs.iter().find(|i| d.name == i.name || d.name.ends_with(&i.name));
        let label = os
            .and_then(|i| i.friendly_name.clone().or(i.description.clone()))
            .or(d.desc.clone())
            .unwrap_or_else(|| d.name.clone());
        let mac = os.and_then(|i| i.mac_addr).map(|m| m.octets()).filter(|m| *m != [0; 6]);
        out.push(NetIf { pcap_name: d.name.clone(), label, mac });
    }
    Ok(out)
}

pub fn resolve_interface(query: &str) -> anyhow::Result<NetIf> {
    if query.is_empty() {
        return Err(anyhow!("Keine AVB-Netzwerkschnittstelle konfiguriert (avb_interface)"));
    }
    let q = query.to_lowercase();
    let all = list_interfaces()?;
    let found = all
        .iter()
        .find(|i| i.pcap_name == query)
        .or_else(|| all.iter().find(|i| i.label.to_lowercase() == q))
        .or_else(|| all.iter().find(|i| i.label.to_lowercase().contains(&q) || i.pcap_name.to_lowercase().contains(&q)))
        .cloned()
        .ok_or_else(|| anyhow!("Schnittstelle '{query}' nicht gefunden – siehe `list-interfaces`"))?;
    if found.mac.is_none() {
        return Err(anyhow!("MAC-Adresse von '{}' konnte nicht ermittelt werden", found.label));
    }
    Ok(found)
}

/// Zeitstempelquelle für pcap: "default", "host_hiprec" oder "adapter" (NIC-Hardware,
/// nur mit passender Karte/Treiber, z.B. Intel i210/i225 + aktuelles Npcap – experimentell).
fn tstamp(kind: &str) -> Option<pcap::TimestampType> {
    match kind {
        "host_hiprec" => Some(pcap::TimestampType::HostHighPrec),
        "adapter" => Some(pcap::TimestampType::Adapter),
        "default" => None,
        // Windows: Npcap-Default basiert auf QueryPerformanceCounter und driftet gegen die
        // Systemuhr – HOST_HIPREC (KeQuerySystemTimePrecise) bleibt synchron.
        _ if cfg!(target_os = "windows") => Some(pcap::TimestampType::HostHighPrec),
        _ => None,
    }
}

fn open(name: &str, timeout_ms: i32, ts_kind: &str) -> anyhow::Result<(Capture<Active>, bool)> {
    let mk = || {
        Capture::from_device(name).map(|c| {
            let c = c.promisc(true).snaplen(2048).immediate_mode(true).timeout(timeout_ms).buffer_size(4 << 20);
            match tstamp(ts_kind) {
                Some(t) => c.tstamp_type(t),
                None => c,
            }
        })
    };
    // Nanosekunden-Zeitstempel, falls die Plattform sie liefert
    if let Ok(c) = mk()?.precision(pcap::Precision::Nano).open() {
        return Ok((c, true));
    }
    Ok((mk()?.open().with_context(|| format!("pcap: '{name}' kann nicht geöffnet werden (Rechte?)"))?, false))
}

pub struct Tx {
    cap: Mutex<Capture<Active>>,
    pub mac: Mac,
    pub sent: AtomicU64,
    pub errors: AtomicU64,
}

impl Tx {
    #[inline]
    pub fn send(&self, frame: &[u8]) -> bool {
        match self.cap.lock().sendpacket(frame) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(e) => {
                if self.errors.fetch_add(1, Ordering::Relaxed) % 1000 == 0 {
                    log::error!("Senden fehlgeschlagen: {e}");
                }
                false
            }
        }
    }
}

pub struct Rx {
    pub cap: Capture<Active>,
    pub nano: bool,
}

pub fn ts_ns(nano: bool, h: &pcap::PacketHeader) -> i64 {
    let sub = h.ts.tv_usec as i64;
    h.ts.tv_sec as i64 * 1_000_000_000 + if nano { sub } else { sub * 1000 }
}

pub fn open_interface(netif: &NetIf, ts_kind: &str) -> anyhow::Result<(Tx, Rx)> {
    let (tx, _) = open(&netif.pcap_name, 1000, "default")?;
    let (mut rx, nano) = open(&netif.pcap_name, 2, ts_kind)?;
    // Eigene gesendete gPTP-Frames werden mitgelesen (Sendezeitstempel für Two-Step),
    // alle anderen eigenen Frames (v.a. 16000 Audio-Pakete/s) ausgefiltert.
    let me = fmt_mac(netif.mac.as_ref().unwrap()).to_lowercase();
    rx.filter(
        &format!(
            "ether proto 0x88f7 or (not ether src {me} and (ether proto 0x22f0 or ether proto 0x22ea \
             or ether proto 0x88f5 or (vlan and ether proto 0x22f0)))"
        ),
        true,
    )?;
    let _ = rx.direction(pcap::Direction::InOut);
    Ok((
        Tx { cap: Mutex::new(tx), mac: netif.mac.unwrap(), sent: AtomicU64::new(0), errors: AtomicU64::new(0) },
        Rx { cap: rx, nano },
    ))
}

// ---------- Byte-Helfer ----------

pub fn eth_header(buf: &mut Vec<u8>, dst: &Mac, src: &Mac, ethertype: u16) {
    buf.extend_from_slice(dst);
    buf.extend_from_slice(src);
    buf.extend_from_slice(&ethertype.to_be_bytes());
}

pub fn eth_header_vlan(buf: &mut Vec<u8>, dst: &Mac, src: &Mac, pcp: u8, vid: u16, ethertype: u16) {
    buf.extend_from_slice(dst);
    buf.extend_from_slice(src);
    buf.extend_from_slice(&ETH_VLAN.to_be_bytes());
    buf.extend_from_slice(&(((pcp as u16) << 13) | (vid & 0x0FFF)).to_be_bytes());
    buf.extend_from_slice(&ethertype.to_be_bytes());
}

/// Liefert (Ethertype, Payload) – VLAN-Tag wird übersprungen.
pub fn parse_eth(frame: &[u8]) -> Option<(Mac, Mac, u16, &[u8])> {
    if frame.len() < 14 {
        return None;
    }
    let dst: Mac = frame[0..6].try_into().ok()?;
    let src: Mac = frame[6..12].try_into().ok()?;
    let mut et = u16::from_be_bytes([frame[12], frame[13]]);
    let mut off = 14;
    if et == ETH_VLAN {
        if frame.len() < 18 {
            return None;
        }
        et = u16::from_be_bytes([frame[16], frame[17]]);
        off = 18;
    }
    Some((dst, src, et, &frame[off..]))
}

pub fn mac_to_u64(m: &Mac) -> u64 {
    m.iter().fold(0u64, |a, b| (a << 8) | *b as u64)
}
pub fn u64_to_mac(v: u64) -> Mac {
    let b = v.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}
pub fn fmt_mac(m: &Mac) -> String {
    m.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}
/// EUI-64 aus MAC (für Entity-ID / gPTP-ClockIdentity).
pub fn eui64(m: &Mac) -> u64 {
    u64::from_be_bytes([m[0], m[1], m[2], 0xFF, 0xFE, m[3], m[4], m[5]])
}

#[inline]
pub fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
#[inline]
pub fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
pub fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}
