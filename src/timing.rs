//! Zeitbasis und Sample-Clock-Schätzung.
//!
//! Alle lokalen Zeitstempel sind Nanosekunden der System-Uhr (UNIX-Epoche),
//! damit sie mit den pcap-Empfangszeitstempeln vergleichbar sind.

use std::time::{SystemTime, UNIX_EPOCH};

#[inline]
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Delay-Locked-Loop (nach F. Adriaensen), schätzt aus verrauschten
/// (Sample-Index, Ankunftszeit)-Paaren der Audio-Callbacks eine glatte
/// Abbildung Sample-Index -> lokale Zeit. Damit folgt die AVB-Media-Clock
/// exakt der Dante-Clock (keine Sample-Rate-Konvertierung nötig).
#[derive(Clone, Debug)]
pub struct SampleClockDll {
    k_base: u64,
    /// Bezugszeitpunkt (ns); t_base ist relativ dazu, damit f64 präzise bleibt
    epoch: i64,
    t_base: f64,
    period: f64, // ns pro Sample
    nominal: f64,
    bw_hz: f64,
    initialized: bool,
    updates: u64,
}

impl SampleClockDll {
    pub fn new(sample_rate: u32) -> Self {
        let nominal = 1e9 / sample_rate as f64;
        Self { k_base: 0, epoch: 0, t_base: 0.0, period: nominal, nominal, bw_hz: 0.05, initialized: false, updates: 0 }
    }

    /// `k` = Index des ersten Samples NACH dem Block, `t` = Ankunftszeit.
    pub fn update(&mut self, k: u64, t: i64) {
        if !self.initialized || k <= self.k_base {
            self.reset(k, t);
            return;
        }
        let n = (k - self.k_base) as f64;
        let pred = self.t_base + n * self.period;
        let err = (t - self.epoch) as f64 - pred;
        // Grobe Ausreißer (z.B. Dropout, Uhrsprung) => neu einrasten
        if err.abs() > 50e6 {
            log::warn!("Sample-Clock: Sprung von {:.1} ms – neu synchronisiert", err / 1e6);
            self.reset(k, t);
            return;
        }
        // Schnell einrasten (1 Hz), danach schmalbandig (0.05 Hz) für eine ruhige Media-Clock
        let bw = if self.updates < 300 { 1.0 } else { self.bw_hz };
        let b = 2.0 * std::f64::consts::PI * bw * n * self.period * 1e-9;
        let w1 = std::f64::consts::SQRT_2 * b;
        let w2 = b * b;
        self.t_base = pred + w1 * err;
        self.period += w2 * err / n;
        // Plausibilitätsgrenze ±0.1 %
        self.period = self.period.clamp(self.nominal * 0.999, self.nominal * 1.001);
        self.k_base = k;
        self.updates += 1;
        if self.t_base > 1e9 {
            let shift = self.t_base.floor();
            self.epoch += shift as i64;
            self.t_base -= shift;
        }
    }

    fn reset(&mut self, k: u64, t: i64) {
        self.k_base = k;
        self.epoch = t;
        self.t_base = 0.0;
        self.period = self.nominal;
        self.initialized = true;
        self.updates = 0;
    }

    pub fn is_locked(&self) -> bool {
        self.initialized && self.updates > 50
    }

    /// Lokale Zeit (ns) des Samples `k`.
    pub fn time_of(&self, k: u64) -> i64 {
        self.epoch + (self.t_base + (k as f64 - self.k_base as f64) * self.period).round() as i64
    }

    /// Geschätzte tatsächliche Sample-Rate.
    pub fn rate(&self) -> f64 {
        1e9 / self.period
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dll_tracks_drifting_clock_with_jitter() {
        let true_rate = 48_004.0; // +83 ppm
        let mut d = SampleClockDll::new(48000);
        let t0 = 1_000_000_000_000i64;
        let mut seed = 1u64;
        for i in 1..30000u64 {
            let k = i * 64;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let jitter = ((seed >> 33) % 400_000) as i64 - 200_000; // ±200 µs
            let t = t0 + (k as f64 * 1e9 / true_rate) as i64 + jitter;
            d.update(k, t);
        }
        assert!(d.is_locked());
        assert!((d.rate() - true_rate).abs() < 0.1, "rate {}", d.rate());
        let k = 30000 * 64;
        let ideal = t0 + (k as f64 * 1e9 / true_rate) as i64;
        assert!((d.time_of(k) - ideal).abs() < 30_000, "err {}", d.time_of(k) - ideal);
        // 6 Samples müssen auf ~1 ns genau 125 µs * (48000/rate) ergeben (keine f64-Quantisierung)
        let step = d.time_of(k + 6) - d.time_of(k);
        assert!((step as f64 - 6e9 / d.rate()).abs() <= 1.0, "step {step}");
    }
}
