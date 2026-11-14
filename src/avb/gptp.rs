//! gPTP (IEEE 802.1AS) – Endpoint als reiner Slave.
//!
//! Zeitstempel kommen von pcap (Software/Treiber). Damit der Nachbar (Switch
//! oder Verstärker mit integriertem AVB-Switch) keine zu große Link-Delay misst
//! (> neighborPropDelayThresh, i.d.R. 800 ns -> Port nicht "asCapable"), werden
//! zwei Maßnahmen kombiniert:
//!
//!  1. Two-Step mit echten Sendezeitstempeln: die eigenen gesendeten
//!     Pdelay-Frames werden per pcap zurückgelesen; deren Treiber-Zeitstempel
//!     ersetzt "Uhrzeit vor dem Senden".
//!  2. Turnaround-Kompensation: Der Fehler, den der Nachbar in seiner Messung
//!     sieht, ist (e_rx + e_tx)/2 – exakt derselbe, den unsere eigene Messung zum
//!     Nachbarn enthält. Wir messen ihn also selbst und rechnen ihn in der
//!     gemeldeten responseOriginTimestamp (t3) heraus.

use crate::net::{self, be16, be64, Tx};
use crate::timing::now_ns;
use std::collections::VecDeque;

const MSG_SYNC: u8 = 0x0;
const MSG_PDELAY_REQ: u8 = 0x2;
const MSG_PDELAY_RESP: u8 = 0x3;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_PDELAY_RESP_FU: u8 = 0xA;
const MSG_ANNOUNCE: u8 = 0xB;

pub struct Gptp {
    clock_id: u64,
    port: u16,
    pdelay_seq: u16,
    pd_t1: Option<(u16, i64)>,
    pd_t2_t4: Option<(u16, i64, i64)>,
    pub link_delay_ns: Option<f64>,
    sync_rx: Option<(u16, i64)>,
    samples: VecDeque<(i64, i64)>, // (lokal, gPTP)
    fit: Option<(i64, f64, f64)>,  // (x0, a, b): gptp = l + a + b*(l-x0)
    last_sync_local: i64,
    pub gm_id: u64,
    pub as_path: Vec<u64>,
    /// gPTP-Port-Identität des direkten Nachbarn (aus dessen Pdelay_Req)
    pub neighbor_port: Option<(u64, u16)>,
    pub syncs: u64,
    pub pdelay_responses_sent: u64,
    /// offene Pdelay_Resp_Follow_Up: (seq, requestingPortIdentity, Ersatz-t3, Deadline)
    pending_fu: Option<(u16, [u8; 10], i64, i64)>,
    pub comp: Compensation,
    pub turnaround_comp_ns: f64,
    pub delay_jitter_ns: f64,
    pub pdelay_measurements: u64,
    delay_hist: VecDeque<f64>,
    pub tx_echo_hits: u64,
    pub tx_echo_misses: u64,
}

#[derive(Clone, Debug)]
pub enum Compensation {
    Off,
    /// Regelkreis: so nachführen, dass der Nachbar (per AVDECC GET_AVB_INFO
    /// abgefragt) `assumed_ns` als Link-Delay meldet
    Auto { assumed_ns: f64 },
    Fixed(f64),
}

impl Compensation {
    pub fn parse(s: &str, assumed_ns: u32) -> Self {
        match s.trim() {
            "off" | "0" => Self::Off,
            "" | "auto" => Self::Auto { assumed_ns: assumed_ns as f64 },
            n => n.parse().map(Self::Fixed).unwrap_or(Self::Auto { assumed_ns: assumed_ns as f64 }),
        }
    }
}

fn write_ts(buf: &mut Vec<u8>, ns: i64) {
    let s = (ns.div_euclid(1_000_000_000)) as u64;
    let n = (ns.rem_euclid(1_000_000_000)) as u32;
    buf.extend_from_slice(&s.to_be_bytes()[2..8]);
    buf.extend_from_slice(&n.to_be_bytes());
}
fn read_ts(b: &[u8], o: usize) -> i64 {
    let mut s = [0u8; 8];
    s[2..8].copy_from_slice(&b[o..o + 6]);
    u64::from_be_bytes(s) as i64 * 1_000_000_000 + net::be32(b, o + 6) as i64
}

impl Gptp {
    pub fn new(clock_id: u64) -> Self {
        Self {
            clock_id,
            port: 1,
            pdelay_seq: 0,
            pd_t1: None,
            pd_t2_t4: None,
            link_delay_ns: None,
            sync_rx: None,
            samples: VecDeque::new(),
            fit: None,
            last_sync_local: 0,
            gm_id: 0,
            as_path: Vec::new(),
            neighbor_port: None,
            syncs: 0,
            pdelay_responses_sent: 0,
            pending_fu: None,
            comp: Compensation::Auto { assumed_ns: 400.0 },
            turnaround_comp_ns: 0.0,
            delay_jitter_ns: 0.0,
            pdelay_measurements: 0,
            delay_hist: VecDeque::new(),
            tx_echo_hits: 0,
            tx_echo_misses: 0,
        }
    }

    fn send_follow_up(&mut self, tx: &Tx, seq: u16, req_port: &[u8; 10], t3: i64) {
        match self.comp {
            Compensation::Off => self.turnaround_comp_ns = 0.0,
            Compensation::Fixed(v) => self.turnaround_comp_ns = v,
            Compensation::Auto { .. } => {} // wird von apply_neighbor_report() geregelt
        }
        let mut f = Vec::with_capacity(68);
        self.header(&mut f, &tx.mac, MSG_PDELAY_RESP_FU, 54, 0, seq, 5, 0x7F, 0);
        write_ts(&mut f, t3 + self.turnaround_comp_ns.round() as i64);
        f.extend_from_slice(req_port);
        tx.send(&f);
        self.pdelay_responses_sent += 1;
    }

    /// Neue Link-Delay-Meldung des Nachbarn: Kompensation nachführen.
    /// Der Nachbar misst wahr + Fehler - comp/2  ->  comp += 2*(gemeldet - Ziel)*Verstärkung.
    pub fn apply_neighbor_report(&mut self, reported_ns: f64) {
        if let Compensation::Auto { assumed_ns } = self.comp {
            let step = 2.0 * (reported_ns - assumed_ns) * 0.25;
            self.turnaround_comp_ns = (self.turnaround_comp_ns + step).clamp(0.0, 2_000_000.0);
        }
    }

    /// Regelmäßig aufrufen: Follow-Up senden, falls der Sende-Echo-Zeitstempel ausbleibt.
    pub fn tick(&mut self, tx: &Tx, now: i64) {
        if let Some((seq, port, t3, deadline)) = self.pending_fu {
            if now >= deadline {
                self.pending_fu = None;
                self.tx_echo_misses += 1;
                self.send_follow_up(tx, seq, &port, t3);
            }
        }
    }

    /// Eigener, von pcap zurückgelesener gPTP-Frame mit Sendezeitstempel.
    pub fn on_own_tx(&mut self, p: &[u8], ts: i64, tx: &Tx) {
        if p.len() < 34 || (p[0] >> 4) != 1 {
            return;
        }
        let seq = be16(p, 30);
        match p[0] & 0x0F {
            MSG_PDELAY_RESP => {
                if let Some((s, port, _, _)) = self.pending_fu {
                    if s == seq {
                        self.pending_fu = None;
                        self.tx_echo_hits += 1;
                        self.send_follow_up(tx, seq, &port, ts);
                    }
                }
            }
            MSG_PDELAY_REQ => {
                if let Some((s, _)) = self.pd_t1 {
                    if s == seq {
                        self.pd_t1 = Some((seq, ts));
                    }
                }
            }
            _ => {}
        }
    }

    fn header(&self, buf: &mut Vec<u8>, src: &net::Mac, msg: u8, len: u16, flags: u16, seq: u16, control: u8, log_int: i8, corr: i64) {
        net::eth_header(buf, &net::MAC_GPTP_MSRP, src, net::ETH_GPTP);
        buf.push(0x10 | msg); // majorSdoId=1 (802.1AS)
        buf.push(0x02); // versionPTP 2
        buf.extend_from_slice(&len.to_be_bytes());
        buf.push(0); // domain
        buf.push(0); // minorSdoId
        buf.extend_from_slice(&flags.to_be_bytes());
        buf.extend_from_slice(&corr.to_be_bytes());
        buf.extend_from_slice(&[0; 4]);
        buf.extend_from_slice(&self.clock_id.to_be_bytes());
        buf.extend_from_slice(&self.port.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.push(control);
        buf.push(log_int as u8);
    }

    /// Periodisch (1 s): eigene Pdelay-Messung starten.
    pub fn send_pdelay_req(&mut self, tx: &Tx) {
        self.pdelay_seq = self.pdelay_seq.wrapping_add(1);
        let mut b = Vec::with_capacity(68);
        self.header(&mut b, &tx.mac, MSG_PDELAY_REQ, 54, 0, self.pdelay_seq, 5, 0, 0);
        b.extend_from_slice(&[0; 20]);
        let t1 = now_ns();
        tx.send(&b);
        self.pd_t1 = Some((self.pdelay_seq, t1));
    }

    pub fn on_rx(&mut self, p: &[u8], rx_ts: i64, tx: &Tx) {
        if p.len() < 34 || (p[0] >> 4) != 1 {
            return;
        }
        let msg = p[0] & 0x0F;
        let seq = be16(p, 30);
        let corr_ns = (be64(p, 8) as i64) >> 16;
        match msg {
            MSG_PDELAY_REQ if p.len() >= 54 => {
                // Pdelay-Responder (two-step): t2 = Empfang, t3 = Sendezeitstempel (Echo)
                if let Some((s, port, t3, _)) = self.pending_fu.take() {
                    self.tx_echo_misses += 1;
                    self.send_follow_up(tx, s, &port, t3);
                }
                let req_port: [u8; 10] = p[20..30].try_into().unwrap();
                self.neighbor_port = Some((be64(p, 20), be16(p, 28)));
                let mut r = Vec::with_capacity(68);
                self.header(&mut r, &tx.mac, MSG_PDELAY_RESP, 54, 0x0200, seq, 5, 0x7F, 0);
                write_ts(&mut r, rx_ts);
                r.extend_from_slice(&req_port);
                let t3 = now_ns();
                tx.send(&r);
                // Follow-Up erst, wenn pcap unseren Frame mit Sendezeitstempel zurückmeldet
                self.pending_fu = Some((seq, req_port, t3, t3 + 4_000_000));
            }
            MSG_PDELAY_RESP if p.len() >= 54 => {
                if be64(p, 44) == self.clock_id {
                    if let Some((s, _)) = self.pd_t1 {
                        if s == seq {
                            self.pd_t2_t4 = Some((seq, read_ts(p, 34), rx_ts));
                        }
                    }
                }
            }
            MSG_PDELAY_RESP_FU if p.len() >= 54 => {
                if be64(p, 44) != self.clock_id {
                    return;
                }
                if let (Some((s1, t1)), Some((s2, t2, t4))) = (self.pd_t1, self.pd_t2_t4) {
                    if s1 == seq && s2 == seq {
                        let t3 = read_ts(p, 34) + corr_ns;
                        let d = ((t4 - t1) - (t3 - t2)) as f64 / 2.0;
                        if d > -1e6 && d < 10e6 {
                            // Robuster Median über die letzten 15 Messungen (Ausreißer durch
                            // Scheduler-Latenzen würden einen Mittelwert stark verfälschen)
                            self.delay_hist.push_back(d);
                            if self.delay_hist.len() > 15 {
                                self.delay_hist.pop_front();
                            }
                            let mut v: Vec<f64> = self.delay_hist.iter().copied().collect();
                            v.sort_by(|a, b| a.total_cmp(b));
                            let q = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
                            self.link_delay_ns = Some(q(0.5));
                            self.delay_jitter_ns = (q(0.75) - q(0.25)) / 2.0;
                            self.pdelay_measurements += 1;
                        }
                    }
                }
                self.pd_t1 = None;
                self.pd_t2_t4 = None;
            }
            MSG_SYNC => self.sync_rx = Some((seq, rx_ts)),
            MSG_FOLLOW_UP if p.len() >= 44 => {
                if let Some((s, t_rx)) = self.sync_rx.take() {
                    if s == seq {
                        let origin = read_ts(p, 34);
                        let gm_at_rx = origin + corr_ns + self.link_delay_ns.unwrap_or(0.0) as i64;
                        self.add_sample(t_rx, gm_at_rx);
                    }
                }
            }
            MSG_ANNOUNCE if p.len() >= 64 => {
                self.gm_id = be64(p, 53);
                // Path-Trace-TLV
                if p.len() >= 68 && be16(p, 64) == 0x0008 {
                    let l = be16(p, 66) as usize;
                    self.as_path = p[68..(68 + l).min(p.len())].chunks_exact(8).map(|c| be64(c, 0)).collect();
                }
            }
            _ => {}
        }
    }

    fn add_sample(&mut self, local: i64, gm: i64) {
        if let Some(pred) = self.to_gptp_raw(local) {
            if (pred - gm).abs() > 1_000_000 {
                log::warn!("gPTP: Zeitsprung {:.3} ms – Servo neu gestartet", (gm - pred) as f64 / 1e6);
                self.samples.clear();
            }
        }
        self.samples.push_back((local, gm));
        while self.samples.len() > 32 {
            self.samples.pop_front();
        }
        self.last_sync_local = local;
        self.syncs += 1;
        self.refit();
    }

    /// Lineare Regression (gptp - lokal) über lokal.
    fn refit(&mut self) {
        let n = self.samples.len();
        if n < 2 {
            self.fit = self.samples.front().map(|&(l, g)| (l, (g - l) as f64, 0.0));
            return;
        }
        let x0 = self.samples[0].0;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        let y0 = (self.samples[0].1 - self.samples[0].0) as f64;
        for &(l, g) in &self.samples {
            let x = (l - x0) as f64;
            let y = (g - l) as f64 - y0;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let nf = n as f64;
        let den = nf * sxx - sx * sx;
        let b = if den.abs() > 1e-9 { (nf * sxy - sx * sy) / den } else { 0.0 };
        let a = (sy - b * sx) / nf + y0;
        self.fit = Some((x0, a, b.clamp(-500e-6, 500e-6)));
    }

    fn to_gptp_raw(&self, local: i64) -> Option<i64> {
        self.fit.map(|(x0, a, b)| local + (a + b * (local - x0) as f64) as i64)
    }

    pub fn is_locked(&self, now: i64) -> bool {
        self.samples.len() >= 4 && now - self.last_sync_local < 2_000_000_000
    }

    /// Lokale Zeit -> gPTP-Zeit (ns), nur wenn eingerastet.
    pub fn to_gptp(&self, local: i64) -> Option<i64> {
        if self.is_locked(now_ns()) {
            self.to_gptp_raw(local)
        } else {
            None
        }
    }

    pub fn offset_ns(&self) -> Option<f64> {
        self.fit.map(|(_, a, _)| a)
    }
    pub fn rate_ppm(&self) -> Option<f64> {
        self.fit.map(|(_, _, b)| b * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts_roundtrip() {
        let mut v = Vec::new();
        let t = 1_763_000_000_123_456_789i64;
        write_ts(&mut v, t);
        assert_eq!(v.len(), 10);
        assert_eq!(read_ts(&v, 0), t);
    }
    #[test]
    fn auto_compensation() {
        let mut g = Gptp::new(1);
        g.comp = Compensation::parse("auto", 400);
        // Modell des Nachbarn: misst wahr (300 ns) + 20 µs Fehler - comp/2
        let neighbor = |comp: f64| 300.0 + 20_000.0 - comp / 2.0;
        for _ in 0..80 {
            let r = neighbor(g.turnaround_comp_ns);
            g.apply_neighbor_report(r);
        }
        assert!((neighbor(g.turnaround_comp_ns) - 400.0).abs() < 1.0, "{}", neighbor(g.turnaround_comp_ns));
        assert!(matches!(Compensation::parse("off", 400), Compensation::Off));
        assert!(matches!(Compensation::parse("1234", 400), Compensation::Fixed(v) if v == 1234.0));
    }

    #[test]
    fn servo_follows_offset_and_drift() {
        let mut g = Gptp::new(1);
        let off = 37_000_000_000i64; // GM-Zeit vs. lokale Uhr
        for i in 0..20i64 {
            let l = 1_000_000_000_000 + i * 125_000_000;
            let gm = l + off + (i * 125_000_000) / 100_000; // +10 ppm
            g.add_sample(l, gm);
        }
        let l = 1_000_000_000_000 + 25 * 125_000_000;
        let expect = l + off + (25 * 125_000_000) / 100_000;
        let got = g.to_gptp_raw(l).unwrap();
        assert!((got - expect).abs() < 5, "{}", got - expect);
        assert!((g.rate_ppm().unwrap() - 10.0).abs() < 0.1);
    }
}
