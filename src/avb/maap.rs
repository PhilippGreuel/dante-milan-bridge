//! MAAP (IEEE 1722 Anhang B): dynamische Vergabe von Multicast-Ziel-MACs.

use crate::net::{self, Mac, Tx};
use crate::timing::now_ns;
use rand::Rng;

const BASE: u64 = 0x91E0_F000_0000;
const RANGE: u64 = 0xFE00;
const PROBE: u8 = 1;
const DEFEND: u8 = 2;
const ANNOUNCE: u8 = 3;

#[derive(Debug, PartialEq)]
enum State {
    Probing(u8),
    Defending,
}

pub struct Maap {
    pub start: u64,
    pub count: u16,
    state: State,
    next_ns: i64,
}

impl Maap {
    pub fn new(count: u16) -> Self {
        let mut m = Self { start: 0, count, state: State::Probing(0), next_ns: 0 };
        m.pick();
        m
    }

    fn pick(&mut self) {
        let off = rand::rng().random_range(0..(RANGE - self.count as u64));
        self.start = BASE + off;
        self.state = State::Probing(0);
        self.next_ns = 0;
    }

    pub fn acquired(&self) -> bool {
        self.state == State::Defending
    }

    fn pdu(&self, src: &Mac, msg: u8, conflict: Option<(u64, u16)>) -> Vec<u8> {
        let mut b = Vec::with_capacity(42);
        net::eth_header(&mut b, &net::MAC_MAAP, src, net::ETH_AVTP);
        b.push(0xFE);
        b.push(msg & 0x0F);
        b.extend_from_slice(&((1u16 << 11) | 16).to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&net::u64_to_mac(self.start));
        b.extend_from_slice(&self.count.to_be_bytes());
        let (cs, cc) = conflict.unwrap_or((0, 0));
        b.extend_from_slice(&net::u64_to_mac(cs));
        b.extend_from_slice(&cc.to_be_bytes());
        b
    }

    /// Gibt `true` zurück, wenn sich der Adressbereich geändert hat.
    pub fn tick(&mut self, tx: &Tx) -> bool {
        let now = now_ns();
        if now < self.next_ns {
            return false;
        }
        match self.state {
            State::Probing(n) if n < 3 => {
                tx.send(&self.pdu(&tx.mac, PROBE, None));
                self.state = State::Probing(n + 1);
                self.next_ns = now + 500_000_000 + rand::rng().random_range(0..100_000_000);
                false
            }
            State::Probing(_) => {
                self.state = State::Defending;
                tx.send(&self.pdu(&tx.mac, ANNOUNCE, None));
                self.next_ns = now + 30_000_000_000;
                log::info!("MAAP: Adressbereich {} (+{}) belegt", net::fmt_mac(&net::u64_to_mac(self.start)), self.count);
                true
            }
            State::Defending => {
                tx.send(&self.pdu(&tx.mac, ANNOUNCE, None));
                self.next_ns = now + 30_000_000_000;
                false
            }
        }
    }

    /// Gibt `true` zurück, wenn wir den Bereich aufgeben mussten.
    pub fn on_rx(&mut self, src: &Mac, p: &[u8], tx: &Tx) -> bool {
        if p.len() < 28 || p[0] != 0xFE {
            return false;
        }
        let msg = p[1] & 0x0F;
        let rs = net::mac_to_u64(&p[12..18].try_into().unwrap());
        let rc = net::be16(p, 18) as u64;
        let overlap = rs < self.start + self.count as u64 && self.start < rs + rc;
        if !overlap {
            return false;
        }
        match (&self.state, msg) {
            (State::Defending, PROBE) => {
                let cs = rs.max(self.start);
                let ce = (rs + rc).min(self.start + self.count as u64);
                tx.send(&self.pdu(&tx.mac, DEFEND, Some((cs, (ce - cs) as u16))));
                false
            }
            (State::Probing(_), _) | (State::Defending, ANNOUNCE | DEFEND)
                if msg != PROBE || net::mac_to_u64(src) < net::mac_to_u64(&tx.mac) =>
            {
                log::warn!("MAAP: Adresskonflikt mit {} – neuer Bereich", net::fmt_mac(src));
                self.pick();
                true
            }
            _ => false,
        }
    }
}
