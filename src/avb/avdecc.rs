//! AVDECC (IEEE 1722.1-2021) mit Milan-Erweiterungen – Talker-Entity.
//!
//! * ADP:  Entity-Ankündigung
//! * AECP: AEM-Kommandos (READ_DESCRIPTOR, GET_STREAM_INFO, ...) + Milan MVU
//! * ACMP: PROBE_TX (= CONNECT_TX), DISCONNECT_TX, GET_TX_STATE
//!
//! Die Bridge erscheint im L-Acoustics Network Manager / Hive als Talker mit
//! N STREAM_OUTPUTs à 8 Kanälen (AAF 48 kHz INT32).

use super::{CHANNELS_PER_STREAM, STREAM_FORMAT_AAF_48K_8CH};
use crate::net::{self, be16, be32, be64, Mac};

const SUBTYPE_ADP: u8 = 0xFA;
const SUBTYPE_AECP: u8 = 0xFB;
const SUBTYPE_ACMP: u8 = 0xFC;

// Descriptor-Typen
pub const DT_ENTITY: u16 = 0x0000;
pub const DT_CONFIGURATION: u16 = 0x0001;
pub const DT_AUDIO_UNIT: u16 = 0x0002;
pub const DT_STREAM_OUTPUT: u16 = 0x0006;
pub const DT_AVB_INTERFACE: u16 = 0x0009;
pub const DT_CLOCK_SOURCE: u16 = 0x000A;
pub const DT_LOCALE: u16 = 0x000C;
pub const DT_STRINGS: u16 = 0x000D;
pub const DT_STREAM_PORT_OUTPUT: u16 = 0x000F;
pub const DT_AUDIO_CLUSTER: u16 = 0x0014;
pub const DT_AUDIO_MAP: u16 = 0x0017;
pub const DT_CLOCK_DOMAIN: u16 = 0x0024;

// AEM-Kommandos
const CMD_ACQUIRE_ENTITY: u16 = 0x0000;
const CMD_LOCK_ENTITY: u16 = 0x0001;
const CMD_ENTITY_AVAILABLE: u16 = 0x0002;
const CMD_CONTROLLER_AVAILABLE: u16 = 0x0003;
const CMD_READ_DESCRIPTOR: u16 = 0x0004;
const CMD_SET_CONFIGURATION: u16 = 0x0006;
const CMD_GET_CONFIGURATION: u16 = 0x0007;
const CMD_SET_STREAM_FORMAT: u16 = 0x0008;
const CMD_GET_STREAM_FORMAT: u16 = 0x0009;
const CMD_GET_STREAM_INFO: u16 = 0x000F;
const CMD_SET_NAME: u16 = 0x0010;
const CMD_GET_NAME: u16 = 0x0011;
const CMD_SET_SAMPLING_RATE: u16 = 0x0014;
const CMD_GET_SAMPLING_RATE: u16 = 0x0015;
const CMD_SET_CLOCK_SOURCE: u16 = 0x0016;
const CMD_GET_CLOCK_SOURCE: u16 = 0x0017;
const CMD_START_STREAMING: u16 = 0x0022;
const CMD_STOP_STREAMING: u16 = 0x0023;
const CMD_REGISTER_UNSOL: u16 = 0x0024;
const CMD_DEREGISTER_UNSOL: u16 = 0x0025;
const CMD_GET_AVB_INFO: u16 = 0x0027;
const CMD_GET_AS_PATH: u16 = 0x0028;
const CMD_GET_COUNTERS: u16 = 0x0029;
const CMD_GET_AUDIO_MAP: u16 = 0x002B;

// AECP-Status
const ST_SUCCESS: u8 = 0;
const ST_NOT_IMPLEMENTED: u8 = 1;
const ST_NO_SUCH_DESCRIPTOR: u8 = 2;
const ST_ENTITY_LOCKED: u8 = 3;
const ST_BAD_ARGUMENTS: u8 = 7;
const ST_NOT_SUPPORTED: u8 = 11;

const MVU_PROTOCOL_ID: [u8; 6] = [0x00, 0x1B, 0xC5, 0x0A, 0xC1, 0x00];

// Entity-/Talker-Fähigkeiten
const CAP_AEM_SUPPORTED: u32 = 1 << 3;
const CAP_CLASS_A: u32 = 1 << 8;
const CAP_GPTP: u32 = 1 << 10;
const CAP_AEM_INTERFACE_INDEX_VALID: u32 = 1 << 15;
const TALKER_CAP: u16 = 0x4001; // IMPLEMENTED | AUDIO_SOURCE

const SAMPLE_RATE: u32 = 48_000;

/// Laufzeitinformationen, die der Stack beim Beantworten mitliefert.
pub struct Runtime {
    pub gm_id: u64,
    pub link_delay_ns: u32,
    pub as_capable: bool,
    pub as_path: Vec<u64>,
    pub vid: u16,
    /// pro Stream: (StreamID, Ziel-MAC gültig?, Ziel-MAC, sendet?)
    pub streams: Vec<(u64, bool, Mac, bool)>,
}

pub struct Entity {
    pub entity_id: u64,
    pub model_id: u64,
    pub mac: Mac,
    pub n_streams: u16,
    pub available_index: u32,
    pub entity_name: String,
    pub group_name: String,
    pub stream_names: Vec<String>,
    pub cluster_names: Vec<String>,
    lock: Option<(u64, i64)>,
    /// Wird gesetzt, wenn ein Controller Namen geändert hat (zum Speichern).
    pub names_dirty: bool,
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    fn name(&mut self, s: &str) -> &mut Self {
        let mut n = [0u8; 64];
        let b = s.as_bytes();
        let mut l = b.len().min(64);
        while !s.is_char_boundary(l) {
            l -= 1;
        }
        n[..l].copy_from_slice(&b[..l]);
        self.bytes(&n)
    }
}

fn read_name(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

impl Entity {
    pub fn new(mac: Mac, n_streams: u16, entity_name: &str, group_name: &str, output_names: &[String]) -> Self {
        let n_ch = n_streams as usize * CHANNELS_PER_STREAM;
        Self {
            entity_id: net::eui64(&mac),
            // Model-ID enthält die Stream-Anzahl, damit Controller-Caches (Hive) nicht kollidieren
            model_id: 0x02DA_4E7E_0000_0000 | n_streams as u64,
            mac,
            n_streams,
            available_index: 0,
            entity_name: entity_name.into(),
            group_name: group_name.into(),
            stream_names: (0..n_streams).map(|i| format!("Out {}-{}", i * 8 + 1, i * 8 + 8)).collect(),
            cluster_names: (0..n_ch).map(|i| output_names.get(i).cloned().unwrap_or_else(|| format!("AVB {}", i + 1))).collect(),
            lock: None,
            names_dirty: false,
        }
    }

    pub fn stream_id(&self, idx: u16) -> u64 {
        (net::mac_to_u64(&self.mac) << 16) | idx as u64
    }

    // ------------------------------------------------------------ ADP

    pub fn adp_available(&mut self, gm_id: u64) -> Vec<u8> {
        self.available_index = self.available_index.wrapping_add(1);
        self.adp_frame(0, gm_id)
    }

    pub fn adp_departing(&self) -> Vec<u8> {
        self.adp_frame(1, 0)
    }

    fn adp_frame(&self, msg_type: u8, gm_id: u64) -> Vec<u8> {
        let mut b = Vec::with_capacity(82);
        net::eth_header(&mut b, &net::MAC_AVDECC, &self.mac, net::ETH_AVTP);
        let mut w = W(b);
        w.u8(SUBTYPE_ADP).u8(msg_type)
            .u16((10 << 11) | 56) // valid_time 10*2 s
            .u64(self.entity_id)
            .u64(self.model_id)
            .u32(CAP_AEM_SUPPORTED | CAP_CLASS_A | CAP_GPTP | CAP_AEM_INTERFACE_INDEX_VALID)
            .u16(self.n_streams)
            .u16(TALKER_CAP)
            .u16(0)
            .u16(0)
            .u32(0)
            .u32(self.available_index)
            .u64(gm_id)
            .u8(0) // gptp_domain_number
            .u8(0)
            .u16(0) // current_configuration_index
            .u16(0) // identify_control_index
            .u16(0) // interface_index
            .u64(0) // association_id
            .u32(0);
        w.0
    }

    /// ADP DISCOVER an uns (oder an alle)?
    pub fn is_discover_for_us(&self, p: &[u8]) -> bool {
        p.len() >= 12 && p[0] == SUBTYPE_ADP && (p[1] & 0x0F) == 2 && {
            let id = be64(p, 4);
            id == 0 || id == self.entity_id
        }
    }

    // ------------------------------------------------------------ Descriptoren

    pub fn descriptor(&self, dt: u16, di: u16, rt: &Runtime) -> Option<Vec<u8>> {
        let n = self.n_streams;
        let n_ch = n * CHANNELS_PER_STREAM as u16;
        let mut w = W(Vec::with_capacity(320));
        w.u16(dt).u16(di);
        match (dt, di) {
            (DT_ENTITY, 0) => {
                w.u64(self.entity_id)
                    .u64(self.model_id)
                    .u32(CAP_AEM_SUPPORTED | CAP_CLASS_A | CAP_GPTP | CAP_AEM_INTERFACE_INDEX_VALID)
                    .u16(n)
                    .u16(TALKER_CAP)
                    .u16(0)
                    .u16(0)
                    .u32(0)
                    .u32(self.available_index)
                    .u64(0)
                    .name(&self.entity_name)
                    .u16(0x0000) // vendor_name_string -> STRINGS[0]
                    .u16(0x0001) // model_name_string  -> STRINGS[1]
                    .name(env!("CARGO_PKG_VERSION"))
                    .name(&self.group_name)
                    .name("")
                    .u16(1)
                    .u16(0);
            }
            (DT_CONFIGURATION, 0) => {
                let counts: [(u16, u16); 6] = [
                    (DT_AUDIO_UNIT, 1),
                    (DT_STREAM_OUTPUT, n),
                    (DT_AVB_INTERFACE, 1),
                    (DT_CLOCK_SOURCE, 1),
                    (DT_LOCALE, 1),
                    (DT_CLOCK_DOMAIN, 1),
                ];
                w.name("Default").u16(0xFFFF).u16(counts.len() as u16).u16(74);
                for (t, c) in counts {
                    w.u16(t).u16(c);
                }
            }
            (DT_AUDIO_UNIT, 0) => {
                w.name("Bridge").u16(0xFFFF).u16(0); // clock_domain_index
                w.u16(0).u16(0); // stream input ports
                w.u16(1).u16(0); // stream output ports
                for _ in 0..14 {
                    w.u16(0).u16(0); // ext/int ports, controls, ...
                }
                w.u32(SAMPLE_RATE).u16(144).u16(1).u32(SAMPLE_RATE);
            }
            (DT_STREAM_OUTPUT, i) if i < n => {
                w.name(&self.stream_names[i as usize])
                    .u16(0xFFFF)
                    .u16(0) // clock_domain_index
                    .u16(0x0003) // CLOCK_SYNC_SOURCE | CLASS_A
                    .u64(STREAM_FORMAT_AAF_48K_8CH)
                    .u16(138) // formats_offset
                    .u16(1);
                for _ in 0..4 {
                    w.u64(0).u16(0); // backup_talker_* / backedup_talker
                }
                w.u16(0) // avb_interface_index
                    .u32(2_000_000) // buffer_length (ns)
                    .u16(146) // redundant_offset
                    .u16(0)
                    .u16(0xFFFF) // timing (kein TIMING-Descriptor)
                    .u64(STREAM_FORMAT_AAF_48K_8CH);
            }
            (DT_AVB_INTERFACE, 0) => {
                w.name("AVB")
                    .u16(0xFFFF)
                    .bytes(&self.mac)
                    .u16(0x0006) // GPTP_SUPPORTED | SRP_SUPPORTED
                    .u64(self.entity_id) // clock_identity
                    .u8(255) // priority1 (nie Grandmaster)
                    .u8(248)
                    .u16(0x436A)
                    .u8(0xFE)
                    .u8(255)
                    .u8(0)
                    .u8((-3i8) as u8) // log_sync_interval
                    .u8(0)
                    .u8(0)
                    .u16(1);
            }
            (DT_CLOCK_SOURCE, 0) => {
                w.name("Dante Media Clock")
                    .u16(0xFFFF)
                    .u16(0x0000)
                    .u16(0x0000) // INTERNAL
                    .u64(0)
                    .u16(DT_AUDIO_UNIT)
                    .u16(0);
            }
            (DT_CLOCK_DOMAIN, 0) => {
                w.name("Clock Domain").u16(0xFFFF).u16(0).u16(76).u16(1).u16(0);
            }
            (DT_LOCALE, 0) => {
                w.name("en-US").u16(1).u16(0);
            }
            (DT_STRINGS, 0) => {
                w.name("Open Source").name("Dante-Milan Bridge");
                for _ in 0..5 {
                    w.name("");
                }
            }
            (DT_STREAM_PORT_OUTPUT, 0) => {
                w.u16(0).u16(0).u16(0).u16(0).u16(n_ch).u16(0).u16(n).u16(0);
            }
            (DT_AUDIO_CLUSTER, i) if i < n_ch => {
                w.name(&self.cluster_names[i as usize])
                    .u16(0xFFFF)
                    .u16(0xFFFF) // signal_type: keiner
                    .u16(0)
                    .u16(0)
                    .u32(0)
                    .u32(0)
                    .u16(1)
                    .u8(0x40); // MBLA
            }
            (DT_AUDIO_MAP, i) if i < n => {
                w.u16(8).u16(CHANNELS_PER_STREAM as u16);
                for c in 0..CHANNELS_PER_STREAM as u16 {
                    w.u16(i).u16(c).u16(i * CHANNELS_PER_STREAM as u16 + c).u16(0);
                }
            }
            _ => return None,
        }
        let _ = rt;
        Some(w.0)
    }

    // ------------------------------------------------------------ AECP

    /// Bearbeitet eine AECP-PDU. Rückgabe: Antwort-Frame (unicast an Controller).
    pub fn handle_aecp(&mut self, src: &Mac, p: &[u8], rt: &Runtime, now: i64) -> Option<Vec<u8>> {
        if p.len() < 24 || p[0] != SUBTYPE_AECP || be64(p, 4) != self.entity_id {
            return None;
        }
        let msg_type = p[1] & 0x0F;
        let controller = be64(p, 12);
        match msg_type {
            0 => {
                let cmd = be16(p, 22) & 0x7FFF;
                let (status, payload) = self.aem_command(cmd, controller, &p[24..], rt, now);
                Some(self.aecp_response(src, p, 1, status, &p[22..24], &payload))
            }
            6 if p.len() >= 30 && p[22..28] == MVU_PROTOCOL_ID => {
                let cmd = be16(p, 28) & 0x7FFF;
                let (status, payload) = match cmd {
                    // GET_MILAN_INFO: reserved, protocol_version, features_flags, certification_version
                    0x0000 => (ST_SUCCESS, W(Vec::new()).u16(0).u32(1).u32(0).u32(0).0.clone()),
                    _ => (ST_NOT_IMPLEMENTED, p[30..].to_vec()),
                };
                Some(self.aecp_response(src, p, 7, status, &p[22..30], &payload))
            }
            _ => None,
        }
    }

    fn aecp_response(&self, dst: &Mac, cmd: &[u8], msg_type: u8, status: u8, cmd_hdr: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + payload.len());
        net::eth_header(&mut b, dst, &self.mac, net::ETH_AVTP);
        let cdl = (8 + 2 + cmd_hdr.len() + payload.len()) as u16;
        b.push(SUBTYPE_AECP);
        b.push(msg_type);
        b.extend_from_slice(&(((status as u16) << 11) | (cdl & 0x07FF)).to_be_bytes());
        b.extend_from_slice(&cmd[4..22]); // target, controller, sequence_id
        b.extend_from_slice(cmd_hdr);
        b.extend_from_slice(payload);
        b
    }

    fn aem_command(&mut self, cmd: u16, controller: u64, a: &[u8], rt: &Runtime, now: i64) -> (u8, Vec<u8>) {
        let echo = a.to_vec();
        let need = |n: usize| a.len() >= n;
        let n = self.n_streams;
        match cmd {
            CMD_ACQUIRE_ENTITY => (ST_NOT_SUPPORTED, echo), // Milan: LOCK statt ACQUIRE
            CMD_LOCK_ENTITY if need(16) => {
                let unlock = be32(a, 0) & 1 != 0;
                if let Some((owner, t)) = self.lock {
                    if now - t > 60_000_000_000 {
                        self.lock = None; // Lock läuft nach 60 s ab
                    } else if owner != controller {
                        let mut r = echo;
                        r[4..12].copy_from_slice(&owner.to_be_bytes());
                        return (ST_ENTITY_LOCKED, r);
                    }
                }
                let mut r = echo;
                if unlock {
                    self.lock = None;
                    r[4..12].fill(0);
                } else {
                    self.lock = Some((controller, now));
                    r[4..12].copy_from_slice(&controller.to_be_bytes());
                }
                (ST_SUCCESS, r)
            }
            CMD_ENTITY_AVAILABLE | CMD_CONTROLLER_AVAILABLE | CMD_REGISTER_UNSOL | CMD_DEREGISTER_UNSOL => (ST_SUCCESS, echo),
            CMD_READ_DESCRIPTOR if need(8) => {
                let (dt, di) = (be16(a, 4), be16(a, 6));
                match (be16(a, 0), self.descriptor(dt, di, rt)) {
                    (0, Some(d)) => (ST_SUCCESS, W(Vec::new()).u16(0).u16(0).bytes(&d).0.clone()),
                    _ => (ST_NO_SUCH_DESCRIPTOR, echo),
                }
            }
            CMD_GET_CONFIGURATION => (ST_SUCCESS, vec![0, 0, 0, 0]),
            CMD_SET_CONFIGURATION if need(4) => (if be16(a, 2) == 0 { ST_SUCCESS } else { ST_BAD_ARGUMENTS }, echo),
            CMD_GET_STREAM_FORMAT | CMD_SET_STREAM_FORMAT if need(4) => {
                let (dt, di) = (be16(a, 0), be16(a, 2));
                if dt != DT_STREAM_OUTPUT || di >= n {
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let status = if cmd == CMD_SET_STREAM_FORMAT && (!need(12) || be64(a, 4) != STREAM_FORMAT_AAF_48K_8CH) {
                    ST_NOT_SUPPORTED
                } else {
                    ST_SUCCESS
                };
                (status, W(Vec::new()).u16(dt).u16(di).u64(STREAM_FORMAT_AAF_48K_8CH).0.clone())
            }
            CMD_GET_STREAM_INFO if need(4) => {
                let (dt, di) = (be16(a, 0), be16(a, 2));
                match rt.streams.get(di as usize) {
                    Some(&(sid, dest_ok, dest, streaming)) if dt == DT_STREAM_OUTPUT => {
                        let mut flags: u32 = (1 << 31) | (1 << 30) | (1 << 29) | (1 << 25); // FORMAT/ID/ACC_LAT/VLAN gültig
                        if dest_ok {
                            flags |= 1 << 28;
                        }
                        if streaming {
                            flags |= 1 << 26; // CONNECTED
                        }
                        let mut w = W(Vec::new());
                        w.u16(dt).u16(di).u32(flags).u64(STREAM_FORMAT_AAF_48K_8CH).u64(sid).u32(500)
                            .bytes(&dest).u8(0).u8(0).u64(0).u16(rt.vid).u16(0)
                            // Milan-Erweiterung: flags_ex (REGISTERING), pbsta/acmpsta, reserviert
                            .u32(1).u8(0).u8(0).u16(0);
                        (ST_SUCCESS, w.0)
                    }
                    _ => (ST_NO_SUCH_DESCRIPTOR, echo),
                }
            }
            CMD_SET_NAME | CMD_GET_NAME if need(8) => {
                let (dt, di, ni) = (be16(a, 0), be16(a, 2), be16(a, 4));
                if cmd == CMD_SET_NAME {
                    if !need(72) {
                        return (ST_BAD_ARGUMENTS, echo);
                    }
                    let name = read_name(&a[8..72]);
                    let ok = match (dt, ni) {
                        (DT_ENTITY, 0) if di == 0 => {
                            self.entity_name = name;
                            true
                        }
                        (DT_ENTITY, 1) if di == 0 => {
                            self.group_name = name;
                            true
                        }
                        (DT_STREAM_OUTPUT, 0) if di < n => {
                            self.stream_names[di as usize] = name;
                            true
                        }
                        (DT_AUDIO_CLUSTER, 0) if (di as usize) < self.cluster_names.len() => {
                            self.cluster_names[di as usize] = name;
                            true
                        }
                        _ => false,
                    };
                    if ok {
                        self.names_dirty = true;
                        return (ST_SUCCESS, echo);
                    }
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let name = match (dt, ni) {
                    (DT_ENTITY, 0) if di == 0 => Some(self.entity_name.clone()),
                    (DT_ENTITY, 1) if di == 0 => Some(self.group_name.clone()),
                    (DT_STREAM_OUTPUT, 0) if di < n => Some(self.stream_names[di as usize].clone()),
                    (DT_AUDIO_CLUSTER, 0) => self.cluster_names.get(di as usize).cloned(),
                    _ => None,
                };
                match name {
                    Some(s) => (ST_SUCCESS, W(a[..8].to_vec()).name(&s).0.clone()),
                    None => (ST_NO_SUCH_DESCRIPTOR, echo),
                }
            }
            CMD_GET_SAMPLING_RATE | CMD_SET_SAMPLING_RATE if need(4) => {
                if be16(a, 0) != DT_AUDIO_UNIT || be16(a, 2) != 0 {
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let st = if cmd == CMD_SET_SAMPLING_RATE && (!need(8) || be32(a, 4) & 0x1FFF_FFFF != SAMPLE_RATE) {
                    ST_NOT_SUPPORTED
                } else {
                    ST_SUCCESS
                };
                (st, W(a[..4].to_vec()).u32(SAMPLE_RATE).0.clone())
            }
            CMD_GET_CLOCK_SOURCE | CMD_SET_CLOCK_SOURCE if need(4) => {
                if be16(a, 0) != DT_CLOCK_DOMAIN || be16(a, 2) != 0 {
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let st = if cmd == CMD_SET_CLOCK_SOURCE && need(6) && be16(a, 4) != 0 { ST_BAD_ARGUMENTS } else { ST_SUCCESS };
                (st, W(a[..4].to_vec()).u16(0).u16(0).0.clone())
            }
            CMD_START_STREAMING | CMD_STOP_STREAMING => (ST_SUCCESS, echo),
            CMD_GET_AVB_INFO if need(4) => {
                if be16(a, 0) != DT_AVB_INTERFACE || be16(a, 2) != 0 {
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let flags = 0x06 | rt.as_capable as u8; // GPTP_ENABLED | SRP_ENABLED | AS_CAPABLE
                let mut w = W(a[..4].to_vec());
                w.u64(rt.gm_id).u32(rt.link_delay_ns).u8(0).u8(flags).u16(1)
                    .u8(super::SR_CLASS_A_ID).u8(super::SR_CLASS_A_PCP).u16(rt.vid);
                (ST_SUCCESS, w.0)
            }
            CMD_GET_AS_PATH if need(2) => {
                let mut w = W(Vec::new());
                w.u16(be16(a, 0)).u16(rt.as_path.len() as u16);
                for id in &rt.as_path {
                    w.u64(*id);
                }
                (ST_SUCCESS, w.0)
            }
            CMD_GET_COUNTERS if need(4) => {
                let mut w = W(a[..4].to_vec());
                w.u32(0).bytes(&[0; 128]);
                (ST_SUCCESS, w.0)
            }
            CMD_GET_AUDIO_MAP if need(8) => {
                let (dt, di, mi) = (be16(a, 0), be16(a, 2), be16(a, 4));
                if dt != DT_STREAM_PORT_OUTPUT || di != 0 || mi >= n {
                    return (ST_NO_SUCH_DESCRIPTOR, echo);
                }
                let mut w = W(Vec::new());
                w.u16(dt).u16(di).u16(mi).u16(n).u16(CHANNELS_PER_STREAM as u16).u16(0);
                for c in 0..CHANNELS_PER_STREAM as u16 {
                    w.u16(mi).u16(c).u16(mi * CHANNELS_PER_STREAM as u16 + c).u16(0);
                }
                (ST_SUCCESS, w.0)
            }
            _ => (ST_NOT_IMPLEMENTED, echo),
        }
    }

    // ------------------------------------------------------------ ACMP

    /// Bearbeitet ACMP (Talker-Seite). Antwort geht per Multicast.
    /// Gibt zusätzlich den Stream-Index bei PROBE_TX zurück.
    pub fn handle_acmp(&self, p: &[u8], rt: &Runtime) -> Option<(Vec<u8>, Option<u16>)> {
        if p.len() < 56 || p[0] != SUBTYPE_ACMP || be64(p, 20) != self.entity_id {
            return None;
        }
        let msg = p[1] & 0x0F;
        let uid = be16(p, 36);
        let resp_type = match msg {
            0 | 2 | 4 => msg + 1, // PROBE_TX(CONNECT_TX), DISCONNECT_TX, GET_TX_STATE
            12 => 13,             // GET_TX_CONNECTION
            _ => return None,
        };
        let mut r = Vec::with_capacity(72);
        net::eth_header(&mut r, &net::MAC_AVDECC, &self.mac, net::ETH_AVTP);
        let start = r.len();
        r.extend_from_slice(&p[..56]);
        let a = &mut r[start..];
        a[1] = resp_type;
        let mut status = 0u8;
        let mut probed = None;
        match rt.streams.get(uid as usize) {
            None => status = 4, // TALKER_NO_STREAM_INDEX
            Some(&(sid, dest_ok, dest, _)) => {
                if msg == 12 {
                    status = 31; // NOT_SUPPORTED
                } else {
                    a[4..12].copy_from_slice(&sid.to_be_bytes());
                    a[40..46].copy_from_slice(&dest);
                    a[52..54].copy_from_slice(&rt.vid.to_be_bytes());
                    a[46..48].copy_from_slice(&0u16.to_be_bytes());
                    if msg == 0 {
                        if dest_ok {
                            probed = Some(uid);
                        } else {
                            status = 3; // TALKER_DEST_MAC_FAIL (MAAP noch nicht fertig)
                        }
                    }
                }
            }
        }
        let cdl = be16(a, 2) & 0x07FF;
        a[2..4].copy_from_slice(&(((status as u16) << 11) | cdl).to_be_bytes());
        Some((r, probed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> Runtime {
        Runtime {
            gm_id: 0x1122,
            link_delay_ns: 300,
            as_capable: true,
            as_path: vec![1, 2],
            vid: 2,
            streams: vec![(0xAA, true, [0x91, 0xE0, 0xF0, 0, 0, 1], true), (0xAB, true, [0x91, 0xE0, 0xF0, 0, 0, 2], false)],
        }
    }

    #[test]
    fn descriptor_sizes_match_1722_1() {
        let e = Entity::new([0, 1, 2, 3, 4, 5], 2, "Bridge", "", &[]);
        let r = rt();
        let len = |t, i| e.descriptor(t, i, &r).unwrap().len();
        assert_eq!(len(DT_ENTITY, 0), 312);
        assert_eq!(len(DT_CONFIGURATION, 0), 74 + 6 * 4);
        assert_eq!(len(DT_AUDIO_UNIT, 0), 144 + 4);
        assert_eq!(len(DT_STREAM_OUTPUT, 1), 138 + 8);
        assert_eq!(len(DT_AVB_INTERFACE, 0), 98);
        assert_eq!(len(DT_CLOCK_SOURCE, 0), 86);
        assert_eq!(len(DT_CLOCK_DOMAIN, 0), 78);
        assert_eq!(len(DT_LOCALE, 0), 72);
        assert_eq!(len(DT_STRINGS, 0), 452);
        assert_eq!(len(DT_STREAM_PORT_OUTPUT, 0), 20);
        assert_eq!(len(DT_AUDIO_CLUSTER, 15), 87);
        assert_eq!(len(DT_AUDIO_MAP, 1), 8 + 8 * 8);
        assert!(e.descriptor(DT_STREAM_OUTPUT, 2, &r).is_none());
        assert!(e.descriptor(DT_AUDIO_CLUSTER, 16, &r).is_none());
    }

    fn aem_cmd(e: &Entity, cmd: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![SUBTYPE_AECP, 0];
        p.extend_from_slice(&((12 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(&e.entity_id.to_be_bytes());
        p.extend_from_slice(&0xC0FFEEu64.to_be_bytes());
        p.extend_from_slice(&7u16.to_be_bytes());
        p.extend_from_slice(&cmd.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn read_entity_descriptor() {
        let mut e = Entity::new([0, 1, 2, 3, 4, 5], 2, "Bridge", "", &[]);
        let p = aem_cmd(&e, CMD_READ_DESCRIPTOR, &[0, 0, 0, 0, 0, 0, 0, 0]);
        let f = e.handle_aecp(&[2; 6], &p, &rt(), 0).unwrap();
        let a = &f[14..];
        assert_eq!(a[1], 1); // AEM_RESPONSE
        assert_eq!(a[2] >> 3, 0); // SUCCESS
        assert_eq!(be16(a, 2) as usize & 0x7FF, a.len() - 12);
        assert_eq!(be16(a, 20), 7);
        assert_eq!(a.len(), 24 + 4 + 312);
        assert_eq!(be64(a, 24 + 8), e.entity_id);
    }

    #[test]
    fn set_and_get_name() {
        let mut e = Entity::new([0, 1, 2, 3, 4, 5], 2, "Bridge", "", &[]);
        let mut pl = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let mut nm = [0u8; 64];
        nm[..5].copy_from_slice(b"FOH 1");
        pl.extend_from_slice(&nm);
        e.handle_aecp(&[2; 6], &aem_cmd(&e, CMD_SET_NAME, &pl), &rt(), 0).unwrap();
        assert_eq!(e.entity_name, "FOH 1");
        assert!(e.names_dirty);
    }

    #[test]
    fn acmp_probe_tx() {
        let e = Entity::new([0, 1, 2, 3, 4, 5], 2, "Bridge", "", &[]);
        let mut p = vec![0u8; 56];
        p[0] = SUBTYPE_ACMP;
        p[1] = 0; // PROBE_TX_COMMAND
        p[2..4].copy_from_slice(&44u16.to_be_bytes());
        p[20..28].copy_from_slice(&e.entity_id.to_be_bytes());
        p[36..38].copy_from_slice(&1u16.to_be_bytes());
        let (f, probed) = e.handle_acmp(&p, &rt()).unwrap();
        let a = &f[14..];
        assert_eq!(a[1], 1);
        assert_eq!(a[2] >> 3, 0);
        assert_eq!(be64(a, 4), 0xAB);
        assert_eq!(&a[40..46], &[0x91, 0xE0, 0xF0, 0, 0, 2]);
        assert_eq!(probed, Some(1));
    }

    #[test]
    fn milan_info() {
        let mut e = Entity::new([0, 1, 2, 3, 4, 5], 1, "B", "", &[]);
        let mut p = vec![SUBTYPE_AECP, 6, 0, 20];
        p.extend_from_slice(&e.entity_id.to_be_bytes());
        p.extend_from_slice(&1u64.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&MVU_PROTOCOL_ID);
        p.extend_from_slice(&[0, 0, 0, 0]);
        let f = e.handle_aecp(&[2; 6], &p, &rt(), 0).unwrap();
        assert_eq!(f[14 + 1], 7);
        assert_eq!(f.len(), 14 + 30 + 14);
        assert_eq!(&f[14 + 22..14 + 28], &MVU_PROTOCOL_ID);
        assert_eq!(be32(&f[14..], 32), 1); // protocol_version
    }
}
