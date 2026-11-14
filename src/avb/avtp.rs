//! IEEE 1722-2016 AAF (AVTP Audio Format) – Paketaufbau.

use crate::net::{self, Mac};

pub const SUBTYPE_AAF: u8 = 0x02;
pub const AAF_FORMAT_INT32: u8 = 0x02;
pub const NSR_48K: u8 = 0x05;

/// Baut einen kompletten Ethernet-Frame (VLAN-getaggt) in `buf`.
/// `samples` = interleaved, channels*SAMPLES_PER_PACKET Werte (i32, MSB-justiert).
#[allow(clippy::too_many_arguments)]
pub fn build_aaf_frame(
    buf: &mut Vec<u8>,
    dst: &Mac,
    src: &Mac,
    vid: u16,
    stream_id: u64,
    seq: u8,
    avtp_ts: u32,
    channels: u16,
    samples: &[i32],
) {
    buf.clear();
    net::eth_header_vlan(buf, dst, src, super::SR_CLASS_A_PCP, vid, net::ETH_AVTP);
    buf.push(SUBTYPE_AAF);
    buf.push(0x81); // sv=1, version=0, mr=0, tv=1
    buf.push(seq);
    buf.push(0x00); // tu=0
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(&avtp_ts.to_be_bytes());
    buf.push(AAF_FORMAT_INT32);
    buf.push((NSR_48K << 4) | ((channels >> 8) as u8 & 0x03));
    buf.push(channels as u8);
    buf.push(32); // bit_depth
    buf.extend_from_slice(&((samples.len() * 4) as u16).to_be_bytes());
    buf.push(0x00); // sp=0 (jedes Paket mit Zeitstempel), evt=0
    buf.push(0x00);
    for s in samples {
        buf.extend_from_slice(&s.to_be_bytes());
    }
}

#[inline]
pub fn f32_to_i32(x: f32) -> i32 {
    let v = (x as f64 * 2147483648.0).round();
    v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aaf_layout() {
        let mut b = Vec::new();
        let s: Vec<i32> = (0..48).collect();
        build_aaf_frame(&mut b, &[0x91, 0xE0, 0xF0, 0, 1, 2], &[2, 0, 0, 0, 0, 1], 2, 0x0200_0000_0001_0000, 7, 0xDEADBEEF, 8, &s);
        assert_eq!(b.len(), 18 + 24 + 192);
        assert_eq!(&b[12..16], &[0x81, 0x00, 0x60, 0x02]); // VLAN PCP3 VID2
        assert_eq!(&b[16..18], &[0x22, 0xF0]);
        let a = &b[18..];
        assert_eq!(a[0], 0x02);
        assert_eq!(a[1], 0x81);
        assert_eq!(a[2], 7);
        assert_eq!(&a[12..16], &0xDEADBEEFu32.to_be_bytes());
        assert_eq!(&a[16..20], &[0x02, 0x50, 0x08, 0x20]);
        assert_eq!(&a[20..22], &[0x00, 0xC0]);
        assert_eq!(&a[28..32], &1i32.to_be_bytes());
        assert_eq!(f32_to_i32(1.5), i32::MAX);
        assert_eq!(f32_to_i32(-1.0), i32::MIN);
    }
}
