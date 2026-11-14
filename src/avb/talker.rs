//! Echtzeit-Talker: liest Dante-Audio aus dem Ringpuffer, routet es auf die
//! AVB-Streams und versendet alle 125 µs ein AAF-Paket pro Stream.
//!
//! Media-Clock: Die AVTP-Presentation-Time jedes Pakets wird aus der (per DLL
//! geglätteten) Ankunftszeit der Dante-Samples abgeleitet und in gPTP-Zeit
//! umgerechnet. Die Milan-Listener (z.B. L-Acoustics LA-Verstärker) rekonstruieren
//! daraus die Media-Clock – die AVB-Seite läuft damit exakt im Dante-Takt.

use super::avtp::{build_aaf_frame, f32_to_i32};
use super::stack::AvbStack;
use super::{CHANNELS_PER_STREAM, SAMPLES_PER_PACKET};
use crate::audio::AudioFeed;
use crate::routing::Router;
use crate::timing::{now_ns, SampleClockDll};
use ringbuf::traits::{Consumer, Observer};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(stack: Arc<AvbStack>, mut feed: AudioFeed, router: Arc<Router>, sample_rate: u32, tx_delay_us: u32, pt_offset_us: u32) -> anyhow::Result<()> {
    std::thread::Builder::new().name("avb-talker".into()).spawn(move || {
        let in_ch = feed.channels;
        let n_streams = stack.streams.len();
        let tx_delay = tx_delay_us as i64 * 1000;
        let pt_offset = pt_offset_us as i64 * 1000;
        let mut dll = SampleClockDll::new(sample_rate);
        let mut k: u64 = 0;
        let mut frames = vec![0f32; in_ch * SAMPLES_PER_PACKET];
        let mut samples = vec![0i32; CHANNELS_PER_STREAM * SAMPLES_PER_PACKET];
        let mut seq = vec![0u8; n_streams];
        let mut frame_buf = Vec::with_capacity(256);
        let mut was_locked = false;
        // Geglätteter Offset gPTP - lokal: verhindert Sprünge in der Presentation-Time
        // bei jedem Servo-Update (Media-Clock-Rekonstruktion der Listener bleibt ruhig)
        let mut off_s: Option<f64> = None;
        if let Err(e) = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max) {
            log::warn!("Talker: Echtzeit-Priorität nicht gesetzt ({e:?}) – höheres Risiko verspäteter Pakete");
        }

        while stack.running.load(Ordering::Relaxed) {
            while let Some((kk, t)) = feed.timing.try_pop() {
                dll.update(kk, t);
            }
            feed.rate_est.store(dll.rate().to_bits(), Ordering::Relaxed);
            let avail = feed.cons.occupied_len() / in_ch;
            if avail < SAMPLES_PER_PACKET {
                std::thread::sleep(Duration::from_micros(200));
                continue;
            }
            if !dll.is_locked() {
                feed.cons.skip(SAMPLES_PER_PACKET * in_ch);
                k += SAMPLES_PER_PACKET as u64;
                continue;
            }
            let send_at = dll.time_of(k) + tx_delay;
            let now = now_ns();
            if now < send_at {
                if send_at - now > 1_500_000 {
                    std::thread::sleep(Duration::from_micros(500));
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }
            feed.cons.pop_slice(&mut frames);
            let this_k = k;
            k += SAMPLES_PER_PACKET as u64;
            let behind = now - send_at;
            if behind > 10_000_000 {
                // >10 ms hinter dem Plan (z.B. nach Hänger) – Pakete verwerfen
                for s in &stack.streams {
                    s.dropped.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            if behind > pt_offset / 2 {
                for s in &stack.streams {
                    s.late.fetch_add(1, Ordering::Relaxed);
                }
            }

            let local = dll.time_of(this_k) + tx_delay;
            let target = stack.gptp.lock().to_gptp(local).map(|g| (g - local) as f64);
            let pt = target.map(|t| {
                let o = match off_s {
                    Some(o) if (t - o).abs() < 1e6 => o + ((t - o) / 2000.0).clamp(-30.0, 30.0),
                    _ => t,
                };
                off_s = Some(o);
                local + o.round() as i64
            });
            if pt.is_some() != was_locked {
                was_locked = pt.is_some();
                log::info!("Talker: gPTP {}", if was_locked { "eingerastet – Streams aktiv" } else { "verloren – Streams angehalten" });
            }
            let Some(pt) = pt else {
                for s in &stack.streams {
                    s.streaming.store(false, Ordering::Relaxed);
                }
                continue;
            };
            let avtp_ts = (pt + pt_offset) as u32;
            let vid = stack.vid.load(Ordering::Relaxed);

            for (si, st) in stack.streams.iter().enumerate() {
                let active = stack.may_stream(si, true);
                if st.streaming.swap(active, Ordering::Relaxed) != active {
                    log::info!("Stream {}: {}", si + 1, if active { "sendet" } else { "gestoppt" });
                }
                if !active {
                    continue;
                }
                for c in 0..CHANNELS_PER_STREAM {
                    let src = router.source(si * CHANNELS_PER_STREAM + c);
                    for f in 0..SAMPLES_PER_PACKET {
                        samples[f * CHANNELS_PER_STREAM + c] = match src {
                            Some(s) => f32_to_i32(frames[f * in_ch + s]),
                            None => 0,
                        };
                    }
                }
                build_aaf_frame(&mut frame_buf, &st.dest(), &stack.mac, vid, st.stream_id, seq[si], avtp_ts, CHANNELS_PER_STREAM as u16, &samples);
                seq[si] = seq[si].wrapping_add(1);
                if stack.tx.send(&frame_buf) {
                    st.packets.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })?;
    Ok(())
}
