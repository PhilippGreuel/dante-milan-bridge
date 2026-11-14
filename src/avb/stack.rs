//! Verdrahtung aller Protokolle: Empfangs-Thread + Housekeeping-Thread.

use super::avdecc::{Entity, Runtime};
use super::gptp::Gptp;
use super::maap::Maap;
use super::mrp::{self, Event, TalkerAdvertise};
use super::*;
use crate::config::Config;
use crate::net::{self, NetIf, Tx};
use crate::timing::now_ns;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct AvbStack {
    pub tx: Arc<Tx>,
    pub mac: net::Mac,
    pub streams: Vec<StreamState>,
    pub gptp: Mutex<Gptp>,
    pub entity: Mutex<Entity>,
    pub maap: Mutex<Maap>,
    pub neighbor: Mutex<super::neighbor::Neighbor>,
    pub vid: AtomicU16,
    pub always_stream: bool,
    pub running: AtomicBool,
    leave_all_at: AtomicI64,
    declarations_sent: AtomicU64,
    pub rx_packets: AtomicU64,
}

impl AvbStack {
    pub fn start(netif: &NetIf, cfg: &Config) -> anyhow::Result<Arc<Self>> {
        let (tx, mut rx) = net::open_interface(netif, &cfg.pcap_timestamps)?;
        let mac = tx.mac;
        let n = cfg.avb_streams.clamp(1, 8);
        let entity = Entity::new(mac, n, &cfg.entity_name, &cfg.group_name, &cfg.output_names);
        let streams = (0..n)
            .map(|i| StreamState {
                stream_id: entity.stream_id(i),
                dest_mac: AtomicU64::new(0),
                listener: AtomicU8::new(LISTENER_NONE),
                listener_seen: AtomicI64::new(0),
                probes: AtomicU16::new(0),
                packets: AtomicU64::new(0),
                late: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                streaming: AtomicBool::new(false),
            })
            .collect();
        log::info!(
            "AVB: Interface {} MAC {} Entity-ID 0x{:016X}, {} Streams, pcap-Zeitstempel {}",
            netif.label,
            net::fmt_mac(&mac),
            entity.entity_id,
            n,
            if rx.nano { "ns" } else { "µs" }
        );
        let stack = Arc::new(Self {
            tx: Arc::new(tx),
            mac,
            streams,
            gptp: Mutex::new({
                let mut g = Gptp::new(net::eui64(&mac));
                g.comp = super::gptp::Compensation::parse(&cfg.pdelay_compensation, cfg.assumed_link_delay_ns);
                g
            }),
            entity: Mutex::new(entity),
            maap: Mutex::new(Maap::new(n)),
            neighbor: Mutex::new(Default::default()),
            vid: AtomicU16::new(DEFAULT_SR_VID),
            always_stream: cfg.always_stream,
            running: AtomicBool::new(true),
            leave_all_at: AtomicI64::new(0),
            declarations_sent: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
        });

        let s = stack.clone();
        let nano = rx.nano;
        std::thread::Builder::new().name("avb-rx".into()).spawn(move || {
            while s.running.load(Ordering::Relaxed) {
                match rx.cap.next_packet() {
                    Ok(pkt) => {
                        let ts = net::ts_ns(nano, pkt.header);
                        s.dispatch(pkt.data, ts);
                        s.gptp.lock().tick(&s.tx, now_ns());
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        s.gptp.lock().tick(&s.tx, now_ns());
                    }
                    Err(e) => {
                        log::error!("pcap Empfang: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        })?;

        let s = stack.clone();
        std::thread::Builder::new().name("avb-housekeeping".into()).spawn(move || s.housekeeping())?;
        Ok(stack)
    }

    pub fn runtime(&self) -> Runtime {
        let g = self.gptp.lock();
        let acquired = self.maap.lock().acquired();
        Runtime {
            gm_id: g.gm_id,
            link_delay_ns: g.link_delay_ns.unwrap_or(0.0) as u32,
            as_capable: g.is_locked(now_ns()) && g.link_delay_ns.is_some(),
            as_path: g.as_path.clone(),
            vid: self.vid.load(Ordering::Relaxed),
            streams: self
                .streams
                .iter()
                .map(|st| (st.stream_id, acquired, st.dest(), st.streaming.load(Ordering::Relaxed)))
                .collect(),
        }
    }

    fn dispatch(&self, frame: &[u8], ts: i64) {
        let Some((_dst, src, et, p)) = net::parse_eth(frame) else { return };
        if p.is_empty() {
            return;
        }
        if src == self.mac {
            // Echo eigener Frames: nur gPTP (Sendezeitstempel)
            if et == net::ETH_GPTP {
                self.gptp.lock().on_own_tx(p, ts, &self.tx);
            }
            return;
        }
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
        match et {
            net::ETH_GPTP => self.gptp.lock().on_rx(p, ts, &self.tx),
            net::ETH_AVTP => match p[0] {
                0xFA => {
                    {
                        let mut nb = self.neighbor.lock();
                        nb.on_adp(&src, p);
                        nb.on_adp_departing(p);
                    }
                    let mut e = self.entity.lock();
                    if e.is_discover_for_us(p) {
                        let gm = self.gptp.lock().gm_id;
                        let f = e.adp_available(gm);
                        drop(e);
                        self.tx.send(&f);
                    }
                }
                0xFB if p.len() > 1 && p[1] & 0x0F == 1 => {
                    let own = self.entity.lock().entity_id;
                    let port = self.gptp.lock().neighbor_port;
                    let mut nb = self.neighbor.lock();
                    let before = nb.reports;
                    nb.on_response(&self.tx, own, port, p, now_ns());
                    if nb.reports != before {
                        let (d, _) = nb.reported.unwrap();
                        drop(nb);
                        self.gptp.lock().apply_neighbor_report(d as f64);
                    }
                }
                0xFB => {
                    let rt = self.runtime();
                    let resp = self.entity.lock().handle_aecp(&src, p, &rt, now_ns());
                    if let Some(f) = resp {
                        self.tx.send(&f);
                    }
                }
                0xFC => {
                    let rt = self.runtime();
                    if let Some((f, probed)) = self.entity.lock().handle_acmp(p, &rt) {
                        self.tx.send(&f);
                        if let Some(i) = probed {
                            self.streams[i as usize].probes.fetch_add(1, Ordering::Relaxed);
                            log::info!("ACMP: Listener hat Stream {} angefragt (PROBE_TX)", i + 1);
                        }
                    }
                }
                0xFE => {
                    let lost = self.maap.lock().on_rx(&src, p, &self.tx);
                    if lost {
                        for s in &self.streams {
                            s.dest_mac.store(0, Ordering::Relaxed);
                        }
                    }
                }
                _ => {}
            },
            net::ETH_MSRP => {
                let r = mrp::parse_msrp(p);
                let now = now_ns();
                for (sid, ev, decl) in r.listeners {
                    if let Some(st) = self.streams.iter().find(|s| s.stream_id == sid) {
                        let new = if ev.is_declared() { decl } else { LISTENER_NONE };
                        let old = st.listener.swap(new, Ordering::Relaxed);
                        if ev.is_declared() {
                            st.listener_seen.store(now, Ordering::Relaxed);
                        }
                        if old != new {
                            log::info!("MSRP: Stream 0x{sid:016X} Listener-Status {old} -> {new}");
                        }
                    }
                }
                for (class, _prio, vid) in r.domains {
                    if class == SR_CLASS_A_ID && vid != 0 && self.vid.swap(vid, Ordering::Relaxed) != vid {
                        log::info!("MSRP: SR-Klasse A nutzt VLAN {vid}");
                    }
                }
                if r.leave_all_listener {
                    self.leave_all_at.store(now, Ordering::Relaxed);
                }
                if r.leave_all {
                    self.declare(Event::JoinIn);
                }
            }
            net::ETH_MVRP if mrp::mvrp_has_leave_all(p) => {
                self.tx.send(&mrp::mvrp_pdu(&self.mac, self.vid.load(Ordering::Relaxed), Event::JoinIn, false));
            }
            _ => {}
        }
    }

    fn talker_advertisements(&self) -> Vec<TalkerAdvertise> {
        if !self.maap.lock().acquired() {
            return vec![];
        }
        let vid = self.vid.load(Ordering::Relaxed);
        self.streams
            .iter()
            .map(|s| TalkerAdvertise {
                stream_id: s.stream_id,
                dest: s.dest(),
                vid,
                max_frame_size: AAF_PDU_SIZE,
                max_interval_frames: 1,
                priority: SR_CLASS_A_PCP,
                rank: 1,
                accumulated_latency: 500,
            })
            .collect()
    }

    fn declare(&self, ev: Event) {
        let vid = self.vid.load(Ordering::Relaxed);
        let talkers = self.talker_advertisements();
        self.tx.send(&mrp::msrp_pdu(&self.mac, vid, &talkers, ev, false));
        self.tx.send(&mrp::mvrp_pdu(&self.mac, vid, ev, false));
        self.declarations_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn housekeeping(&self) {
        let mut next_pdelay = 0i64;
        let mut next_adp = 0i64;
        let mut next_decl = 0i64;
        let mut new_count = 0;
        while self.running.load(Ordering::Relaxed) {
            let now = now_ns();
            {
                let mut m = self.maap.lock();
                if m.tick(&self.tx) {
                    for (i, s) in self.streams.iter().enumerate() {
                        s.dest_mac.store(m.start + i as u64, Ordering::Relaxed);
                    }
                    new_count = 0; // Talker mit "New" neu ankündigen
                    next_decl = 0;
                }
            }
            {
                let own = self.entity.lock().entity_id;
                let port = self.gptp.lock().neighbor_port;
                self.neighbor.lock().tick(&self.tx, own, port, now);
            }
            if now >= next_pdelay {
                self.gptp.lock().send_pdelay_req(&self.tx);
                next_pdelay = now + 1_000_000_000;
            }
            if now >= next_adp {
                let gm = self.gptp.lock().gm_id;
                let f = self.entity.lock().adp_available(gm);
                self.tx.send(&f);
                next_adp = now + 5_000_000_000;
            }
            if now >= next_decl {
                let ev = if new_count < 3 { Event::New } else { Event::JoinIn };
                new_count += 1;
                self.declare(ev);
                next_decl = now + 500_000_000;
            }
            // LeaveAll vom Switch: Listener, die sich nicht neu deklariert haben, entfernen
            let la = self.leave_all_at.load(Ordering::Relaxed);
            if la != 0 && now - la > 1_500_000_000 {
                for s in &self.streams {
                    if s.listener_seen.load(Ordering::Relaxed) < la && s.listener.swap(LISTENER_NONE, Ordering::Relaxed) != LISTENER_NONE {
                        log::info!("MSRP: Listener für Stream 0x{:016X} abgemeldet", s.stream_id);
                    }
                }
                self.leave_all_at.store(0, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn may_stream(&self, idx: usize, gptp_locked: bool) -> bool {
        let s = &self.streams[idx];
        gptp_locked && s.dest_mac.load(Ordering::Relaxed) != 0 && (self.always_stream || s.has_listener())
    }

    /// Sauber abmelden (MSRP Leave, ADP Departing).
    pub fn shutdown(&self) {
        let vid = self.vid.load(Ordering::Relaxed);
        let talkers = self.talker_advertisements();
        self.tx.send(&mrp::msrp_pdu(&self.mac, vid, &talkers, Event::Lv, false));
        let f = self.entity.lock().adp_departing();
        self.tx.send(&f);
        self.running.store(false, Ordering::Relaxed);
    }
}
