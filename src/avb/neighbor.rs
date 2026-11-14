//! Geschlossener Regelkreis für die Pdelay-Kompensation.
//!
//! Der direkte gPTP-Nachbar (z.B. der erste Verstärker der Daisy-Chain) misst
//! die Link-Delay zu uns und meldet sie per AVDECC GET_AVB_INFO
//! (propagation_delay). Wir identifizieren seine AVDECC-Entity über die
//! gPTP-Port-Identität aus seinen Pdelay_Req und fragen den Wert regelmäßig ab.
//! Die Kompensation wird so nachgeführt, dass der Nachbar den Zielwert sieht.

use crate::net::{self, be16, be64, Mac, Tx};
use std::collections::HashMap;

const DT_CONFIGURATION: u16 = 0x0001;
const DT_AVB_INTERFACE: u16 = 0x0009;
const CMD_READ_DESCRIPTOR: u16 = 0x0004;
const CMD_GET_AVB_INFO: u16 = 0x0027;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    Config(u64),
    AvbIf(u64, u16),
    AvbInfo,
}

#[derive(Default)]
pub struct Neighbor {
    /// Entities aus ADP: ID -> (MAC, bereits untersucht?)
    entities: HashMap<u64, (Mac, bool)>,
    /// gefundener Nachbar: (Entity-ID, MAC, AVB_INTERFACE-Index)
    pub target: Option<(u64, Mac, u16)>,
    pending: HashMap<u16, (Pending, i64)>,
    seq: u16,
    next_probe: i64,
    next_query: i64,
    /// zuletzt vom Nachbarn gemeldete Link-Delay (ns) und Zeitpunkt
    pub reported: Option<(u32, i64)>,
    pub reports: u64,
}

impl Neighbor {
    pub fn on_adp(&mut self, src: &Mac, p: &[u8]) {
        if p.len() >= 12 && (p[1] & 0x0F) == 0 {
            self.entities.entry(be64(p, 4)).or_insert((*src, false)).0 = *src;
        }
    }

    pub fn on_adp_departing(&mut self, p: &[u8]) {
        if p.len() >= 12 && (p[1] & 0x0F) == 1 {
            let id = be64(p, 4);
            self.entities.remove(&id);
            if self.target.is_some_and(|t| t.0 == id) {
                self.target = None;
                self.reported = None;
            }
        }
    }

    fn command(&mut self, tx: &Tx, own_id: u64, dst: &Mac, target: u64, cmd: u16, payload: &[u8], what: Pending, now: i64) {
        self.seq = self.seq.wrapping_add(1);
        let mut b = Vec::with_capacity(64);
        net::eth_header(&mut b, dst, &tx.mac, net::ETH_AVTP);
        b.push(0xFB);
        b.push(0x00);
        b.extend_from_slice(&((12 + payload.len()) as u16).to_be_bytes());
        b.extend_from_slice(&target.to_be_bytes());
        b.extend_from_slice(&own_id.to_be_bytes());
        b.extend_from_slice(&self.seq.to_be_bytes());
        b.extend_from_slice(&cmd.to_be_bytes());
        b.extend_from_slice(payload);
        tx.send(&b);
        self.pending.insert(self.seq, (what, now));
    }

    /// Periodisch aufrufen. `port` = gPTP-Port-Identität des Nachbarn (aus Pdelay_Req).
    pub fn tick(&mut self, tx: &Tx, own_id: u64, port: Option<(u64, u16)>, now: i64) {
        self.pending.retain(|_, (_, t)| now - *t < 2_000_000_000);
        if port.is_none() {
            return;
        }
        if let Some((id, mac, idx)) = self.target {
            if now >= self.next_query {
                let pl = [DT_AVB_INTERFACE.to_be_bytes(), idx.to_be_bytes()].concat();
                self.command(tx, own_id, &mac, id, CMD_GET_AVB_INFO, &pl, Pending::AvbInfo, now);
                self.next_query = now + 1_000_000_000;
            }
            return;
        }
        if now < self.next_probe {
            return;
        }
        self.next_probe = now + 300_000_000;
        // nächste noch nicht untersuchte Entity befragen
        if let Some((&id, &(mac, _))) = self.entities.iter().find(|(_, (_, done))| !done) {
            self.entities.get_mut(&id).unwrap().1 = true;
            let pl = [0u16, 0, DT_CONFIGURATION, 0].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>();
            self.command(tx, own_id, &mac, id, CMD_READ_DESCRIPTOR, &pl, Pending::Config(id), now);
        } else if !self.entities.is_empty() {
            // alle untersucht, nichts gefunden: in 10 s erneut versuchen
            for e in self.entities.values_mut() {
                e.1 = false;
            }
            self.next_probe = now + 10_000_000_000;
        }
    }

    /// AECP-Antwort an uns (als Controller).
    pub fn on_response(&mut self, tx: &Tx, own_id: u64, port: Option<(u64, u16)>, p: &[u8], now: i64) {
        if p.len() < 24 || p[2] >> 3 != 0 || be64(p, 12) != own_id {
            return;
        }
        let Some((what, _)) = self.pending.remove(&be16(p, 20)) else { return };
        let a = &p[24..];
        match what {
            Pending::Config(id) if a.len() >= 4 + 74 => {
                let d = &a[4..];
                let n = be16(d, 70) as usize;
                let off = be16(d, 72) as usize;
                for i in 0..n {
                    let o = off + 4 * i;
                    if o + 4 <= d.len() && be16(d, o) == DT_AVB_INTERFACE {
                        let mac = self.entities.get(&id).map(|e| e.0).unwrap_or([0; 6]);
                        for idx in 0..be16(d, o + 2) {
                            let pl = [0u16, 0, DT_AVB_INTERFACE, idx].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>();
                            self.command(tx, own_id, &mac, id, CMD_READ_DESCRIPTOR, &pl, Pending::AvbIf(id, idx), now);
                        }
                    }
                }
            }
            Pending::AvbIf(id, idx) if a.len() >= 4 + 98 => {
                let d = &a[4..];
                let (clock, port_no) = (be64(d, 78), be16(d, 96));
                if port == Some((clock, port_no)) {
                    let mac = self.entities.get(&id).map(|e| e.0).unwrap_or([0; 6]);
                    log::info!("Pdelay-Regelung: Nachbar ist Entity 0x{id:016X}, AVB-Interface {idx}");
                    self.target = Some((id, mac, idx));
                    self.next_query = 0;
                }
            }
            Pending::AvbInfo if a.len() >= 16 => {
                self.reported = Some((net::be32(a, 12), now));
                self.reports += 1;
            }
            _ => {}
        }
    }
}
