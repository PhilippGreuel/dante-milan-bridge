//! MRP-Applikationen MSRP (Stream-Reservierung) und MVRP (VLAN) nach IEEE 802.1Q.
//! Vereinfachter Applicant: Deklarationen werden periodisch als JoinIn
//! gesendet; LeaveAll des Switches wird sofort beantwortet.

use crate::net::{self, be16, be64, Mac};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Event {
    New = 0,
    JoinIn = 1,
    In = 2,
    JoinMt = 3,
    Mt = 4,
    Lv = 5,
}

impl Event {
    fn from(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::New,
            1 => Self::JoinIn,
            2 => Self::In,
            3 => Self::JoinMt,
            4 => Self::Mt,
            5 => Self::Lv,
            _ => return None,
        })
    }
    pub fn is_declared(self) -> bool {
        matches!(self, Self::New | Self::JoinIn | Self::JoinMt | Self::In)
    }
}

pub const ATTR_TALKER_ADVERTISE: u8 = 1;
#[allow(dead_code)]
pub const ATTR_TALKER_FAILED: u8 = 2;
pub const ATTR_LISTENER: u8 = 3;
pub const ATTR_DOMAIN: u8 = 4;

#[derive(Clone, Debug)]
pub struct TalkerAdvertise {
    pub stream_id: u64,
    pub dest: Mac,
    pub vid: u16,
    pub max_frame_size: u16,
    pub max_interval_frames: u16,
    pub priority: u8,
    pub rank: u8,
    pub accumulated_latency: u32,
}

fn vector(buf: &mut Vec<u8>, leave_all: bool, first_value: &[u8], ev: Event) {
    let hdr: u16 = ((leave_all as u16) << 13) | 1;
    buf.extend_from_slice(&hdr.to_be_bytes());
    buf.extend_from_slice(first_value);
    buf.push(ev as u8 * 36); // ThreePackedEvent mit einem Wert
}

fn msrp_message(buf: &mut Vec<u8>, attr_type: u8, attr_len: u8, vectors: &[Vec<u8>]) {
    buf.push(attr_type);
    buf.push(attr_len);
    let list_len: usize = vectors.iter().map(|v| v.len()).sum::<usize>() + 2;
    buf.extend_from_slice(&(list_len as u16).to_be_bytes());
    for v in vectors {
        buf.extend_from_slice(v);
    }
    buf.extend_from_slice(&[0, 0]); // EndMark der AttributeList
}

/// MSRP-PDU mit Domain + Talker-Advertise-Deklarationen.
pub fn msrp_pdu(src: &Mac, domain_vid: u16, talkers: &[TalkerAdvertise], ev: Event, leave_all: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(128);
    net::eth_header(&mut b, &net::MAC_GPTP_MSRP, src, net::ETH_MSRP);
    b.push(0x00); // ProtocolVersion
    // Domain: SRclassID, SRclassPriority, SRclassVID
    let mut dv = Vec::new();
    let mut fv = vec![super::SR_CLASS_A_ID, super::SR_CLASS_A_PCP];
    fv.extend_from_slice(&domain_vid.to_be_bytes());
    vector(&mut dv, leave_all, &fv, ev);
    msrp_message(&mut b, ATTR_DOMAIN, 4, &[dv]);
    if !talkers.is_empty() {
        let vs: Vec<Vec<u8>> = talkers
            .iter()
            .map(|t| {
                let mut fv = Vec::with_capacity(25);
                fv.extend_from_slice(&t.stream_id.to_be_bytes());
                fv.extend_from_slice(&t.dest);
                fv.extend_from_slice(&t.vid.to_be_bytes());
                fv.extend_from_slice(&t.max_frame_size.to_be_bytes());
                fv.extend_from_slice(&t.max_interval_frames.to_be_bytes());
                fv.push((t.priority << 5) | ((t.rank & 1) << 4));
                fv.extend_from_slice(&t.accumulated_latency.to_be_bytes());
                let mut v = Vec::new();
                vector(&mut v, leave_all, &fv, ev);
                v
            })
            .collect();
        msrp_message(&mut b, ATTR_TALKER_ADVERTISE, 25, &vs);
    }
    b.extend_from_slice(&[0, 0]); // EndMark PDU
    b
}

/// MVRP-PDU: VLAN beitreten (Listener-Seite des Switches muss den VLAN-Verkehr zu uns leiten).
pub fn mvrp_pdu(src: &Mac, vid: u16, ev: Event, leave_all: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    net::eth_header(&mut b, &net::MAC_MVRP, src, net::ETH_MVRP);
    b.push(0x00);
    b.push(1); // AttributeType VID
    b.push(2);
    vector(&mut b, leave_all, &vid.to_be_bytes(), ev);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b
}

#[derive(Debug, Default)]
pub struct MsrpRx {
    /// (StreamID, Event, Deklarationstyp)
    pub listeners: Vec<(u64, Event, u8)>,
    pub domains: Vec<(u8, u8, u16)>,
    pub leave_all: bool,
    pub leave_all_listener: bool,
}

pub fn parse_msrp(p: &[u8]) -> MsrpRx {
    let mut r = MsrpRx::default();
    if p.is_empty() {
        return r;
    }
    let mut o = 1; // ProtocolVersion
    while o + 4 <= p.len() {
        if be16(p, o) == 0 {
            break;
        }
        let at = p[o];
        let al = p[o + 1] as usize;
        let list_len = be16(p, o + 2) as usize;
        let end = (o + 4 + list_len).min(p.len());
        let mut v = o + 4;
        while v + 2 <= end {
            let hdr = be16(p, v);
            if hdr == 0 {
                break;
            }
            let la = (hdr >> 13) != 0;
            let n = (hdr & 0x1FFF) as usize;
            let fv = v + 2;
            let three = fv + al;
            let four = three + n.div_ceil(3);
            let vend = four + if at == ATTR_LISTENER { n.div_ceil(4) } else { 0 };
            if vend > end {
                break;
            }
            if la {
                r.leave_all = true;
                if at == ATTR_LISTENER {
                    r.leave_all_listener = true;
                }
            }
            for i in 0..n {
                let pk = p[three + i / 3];
                let e = match i % 3 {
                    0 => pk / 36,
                    1 => (pk / 6) % 6,
                    _ => pk % 6,
                };
                let Some(ev) = Event::from(e) else { continue };
                match at {
                    ATTR_LISTENER if al == 8 => {
                        let d = (p[four + i / 4] >> (6 - 2 * (i % 4))) & 0x03;
                        r.listeners.push((be64(p, fv).wrapping_add(i as u64), ev, d));
                    }
                    ATTR_DOMAIN if al == 4 && i == 0 => {
                        if ev.is_declared() {
                            r.domains.push((p[fv], p[fv + 1], be16(p, fv + 2)));
                        }
                    }
                    _ => {}
                }
            }
            v = vend;
        }
        o = end;
    }
    r
}

/// Prüft nur auf LeaveAll in einer MVRP-PDU.
pub fn mvrp_has_leave_all(p: &[u8]) -> bool {
    p.len() >= 6 && (be16(p, 3) >> 13) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn talker_advertise_encoding() {
        let t = TalkerAdvertise {
            stream_id: 0x0011_2233_4455_0000,
            dest: [0x91, 0xE0, 0xF0, 0, 0x12, 0x34],
            vid: 2,
            max_frame_size: 216,
            max_interval_frames: 1,
            priority: 3,
            rank: 1,
            accumulated_latency: 500,
        };
        let pdu = msrp_pdu(&[0, 0x11, 0x22, 0x33, 0x44, 0x55], 2, &[t], Event::JoinIn, false);
        let p = &pdu[14..];
        assert_eq!(p[0], 0);
        // Domain-Message
        assert_eq!(&p[1..5], &[4, 4, 0, 9]);
        assert_eq!(&p[5..7], &[0, 1]);
        assert_eq!(&p[7..11], &[6, 3, 0, 2]);
        assert_eq!(p[11], 36);
        // Talker-Message
        let t = &p[14..];
        assert_eq!(&t[0..4], &[1, 25, 0, 2 + 25 + 1 + 2]);
        assert_eq!(&t[6..14], &0x0011_2233_4455_0000u64.to_be_bytes());
        assert_eq!(t[4 + 2 + 20], 0x70);
        assert_eq!(&pdu[pdu.len() - 2..], &[0, 0]);
    }

    #[test]
    fn listener_parsing() {
        // Listener-Message: 2 Werte, Stream 0x10 Ready, 0x11 AskingFailed
        let mut p = vec![0u8];
        p.extend_from_slice(&[3, 8]);
        let vec_len = 2 + 8 + 1 + 1;
        p.extend_from_slice(&((vec_len + 2) as u16).to_be_bytes());
        p.extend_from_slice(&[0x20, 0x02]); // LeaveAll + n=2
        p.extend_from_slice(&0x10u64.to_be_bytes());
        p.push(1 * 36 + 3 * 6); // JoinIn, JoinMt
        p.push((2 << 6) | (1 << 4));
        p.extend_from_slice(&[0, 0, 0, 0]);
        let r = parse_msrp(&p);
        assert!(r.leave_all_listener);
        assert_eq!(r.listeners, vec![(0x10, Event::JoinIn, 2), (0x11, Event::JoinMt, 1)]);
    }
}
