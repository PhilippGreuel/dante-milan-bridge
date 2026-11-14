//! Eigener AVB/Milan-Endpoint-Stack (Talker).
//!
//! Komponenten:
//!  * gPTP (IEEE 802.1AS)     – Slave + Pdelay-Responder/-Initiator  (gptp.rs)
//!  * MAAP (IEEE 1722 Anh. B) – Vergabe der Multicast-Ziel-MACs      (maap.rs)
//!  * MSRP/MVRP (802.1Q)      – Stream-Reservierung, VLAN-Beitritt   (mrp.rs)
//!  * AVDECC (1722.1 + Milan) – ADP/AECP(AEM, MVU)/ACMP              (avdecc.rs)
//!  * AVTP AAF (1722)         – Audio-Streams                         (avtp.rs, talker.rs)

pub mod avdecc;
pub mod avtp;
pub mod gptp;
pub mod maap;
pub mod mrp;
pub mod stack;
pub mod talker;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU64, AtomicU8, Ordering};

pub const CHANNELS_PER_STREAM: usize = 8;
pub const SAMPLES_PER_PACKET: usize = 6; // 48 kHz, Class A (125 µs)
pub const SR_CLASS_A_ID: u8 = 6;
pub const SR_CLASS_A_PCP: u8 = 3;
pub const DEFAULT_SR_VID: u16 = 2;
/// AVTPDU-Größe eines 8-Kanal-Streams: 24 Byte Header + 8*4*6 Byte Payload.
pub const AAF_PDU_SIZE: u16 = 24 + (CHANNELS_PER_STREAM * 4 * SAMPLES_PER_PACKET) as u16;
/// Milan Basis-Format: AAF, 48 kHz, INT32, 8 Kanäle, 6 Samples/Frame.
pub const STREAM_FORMAT_AAF_48K_8CH: u64 = 0x0205_0220_0200_6000;

/// MSRP-Listener-Deklaration (FourPackedEvent)
#[allow(dead_code)]
pub const LISTENER_NONE: u8 = 0;
#[allow(dead_code)]
pub const LISTENER_ASKING_FAILED: u8 = 1;
pub const LISTENER_READY: u8 = 2;
pub const LISTENER_READY_FAILED: u8 = 3;

pub struct StreamState {
    pub stream_id: u64,
    pub dest_mac: AtomicU64,
    pub listener: AtomicU8,
    /// Zeitpunkt der letzten Listener-Deklaration (für LeaveAll-Behandlung)
    pub listener_seen: AtomicI64,
    pub probes: AtomicU16,
    pub packets: AtomicU64,
    /// Pakete, die mehr als die halbe Presentation-Reserve zu spät gesendet wurden
    pub late: AtomicU64,
    /// Verworfene Pakete (>10 ms Rückstand)
    pub dropped: AtomicU64,
    pub streaming: AtomicBool,
}

impl StreamState {
    pub fn dest(&self) -> crate::net::Mac {
        crate::net::u64_to_mac(self.dest_mac.load(Ordering::Relaxed))
    }
    pub fn has_listener(&self) -> bool {
        matches!(self.listener.load(Ordering::Relaxed), LISTENER_READY | LISTENER_READY_FAILED)
    }
}
