//! Audio-I/O über cpal: CoreAudio (macOS), WASAPI oder ASIO (Windows).
//! Eingang ist i.d.R. die Dante Virtual Soundcard.

use crate::config::Config;
use crate::routing::Router;
use crate::timing::now_ns;
use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Verbraucher-Seite des Eingangs (für Talker bzw. Ausgabe-Device).
pub struct AudioFeed {
    pub cons: HeapCons<f32>,
    /// (Anzahl bisher geschriebener Frames, Zeitstempel ns) pro Callback
    pub timing: HeapCons<(u64, i64)>,
    pub channels: usize,
    pub rate_est: Arc<AtomicU64>,
}

pub struct AudioStats {
    pub device: String,
    pub channels: usize,
    pub sample_rate: u32,
    pub overflows: Arc<AtomicU64>,
    pub underruns: Arc<AtomicU64>,
    pub rate_est: Arc<AtomicU64>,
    pub callbacks: Arc<AtomicU64>,
}

pub fn host(name: &str) -> anyhow::Result<cpal::Host> {
    match name.to_lowercase().as_str() {
        "" | "default" => Ok(cpal::default_host()),
        #[cfg(all(target_os = "windows", feature = "asio"))]
        "asio" => Ok(cpal::host_from_id(cpal::HostId::Asio)?),
        other => Err(anyhow!("Audio-Host '{other}' nicht verfügbar (ASIO erfordert Windows + Feature `asio`)")),
    }
}

fn find_device(host: &cpal::Host, query: &str, input: bool) -> anyhow::Result<cpal::Device> {
    let q = query.to_lowercase();
    let devs: Vec<_> = if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() };
    devs.into_iter()
        .find(|d| d.to_string().to_lowercase().contains(&q))
        .ok_or_else(|| anyhow!("Audio-Device '{query}' nicht gefunden – siehe `list-audio`"))
}

pub fn list_audio() -> anyhow::Result<()> {
    for id in cpal::available_hosts() {
        let h = cpal::host_from_id(id)?;
        println!("Host: {id:?}");
        for d in h.devices()? {
            let ins = d.supported_input_configs().map(|c| c.map(|r| r.channels()).max().unwrap_or(0)).unwrap_or(0);
            let outs = d.supported_output_configs().map(|c| c.map(|r| r.channels()).max().unwrap_or(0)).unwrap_or(0);
            println!("  \"{d}\"  Eingänge: {ins}  Ausgänge: {outs}");
        }
    }
    Ok(())
}

fn pick_config(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    rate: u32,
    want_ch: u16,
) -> anyhow::Result<cpal::SupportedStreamConfig> {
    let mut best: Option<cpal::SupportedStreamConfigRange> = None;
    let fmt_rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I16 => 3,
        _ => 9,
    };
    for r in ranges {
        if r.min_sample_rate() > rate || r.max_sample_rate() < rate || fmt_rank(r.sample_format()) == 9 {
            continue;
        }
        let better = match &best {
            None => true,
            Some(b) => {
                let ch_ok = |c: u16| want_ch == 0 || c >= want_ch;
                (ch_ok(r.channels()) && !ch_ok(b.channels()))
                    || (ch_ok(r.channels()) == ch_ok(b.channels())
                        && (r.channels() > b.channels()
                            || (r.channels() == b.channels() && fmt_rank(r.sample_format()) < fmt_rank(b.sample_format()))))
            }
        };
        if better {
            best = Some(r);
        }
    }
    best.map(|b| b.with_sample_rate(rate)).ok_or_else(|| anyhow!("Kein passendes Format mit {rate} Hz gefunden"))
}

fn stream_config(sc: &cpal::SupportedStreamConfig, want_ch: u16, buffer: u32) -> cpal::StreamConfig {
    let mut c = sc.config();
    if want_ch > 0 && want_ch < c.channels {
        // Viele Treiber erlauben weniger Kanäle; sonst bleibt die Maximalzahl
        c.channels = want_ch;
    }
    if buffer > 0 {
        c.buffer_size = cpal::BufferSize::Fixed(buffer);
    }
    c
}

/// Hält den Eingang am Leben (cpal-Stream oder Testton-Thread).
pub enum InputGuard {
    Device(#[allow(dead_code)] cpal::Stream),
    TestTone,
}

/// Öffnet das Dante-Eingangs-Device – oder den Testton-Generator, wenn
/// `dante_device` = "test-tone" ist (Sinus 1 kHz, -20 dBFS auf allen Kanälen,
/// Kanal n zusätzlich mit n Impulsen pro Sekunde zur Identifikation).
pub fn open_input(cfg: &Config) -> anyhow::Result<(InputGuard, AudioFeed, AudioStats)> {
    if cfg.dante_device.eq_ignore_ascii_case("test-tone") {
        return Ok(test_tone(cfg));
    }
    let host = host(&cfg.audio_host)?;
    let dev = find_device(&host, &cfg.dante_device, true)?;
    let sc = pick_config(dev.supported_input_configs()?, cfg.sample_rate, cfg.input_channels)?;
    let mut conf = stream_config(&sc, cfg.input_channels, cfg.buffer_frames);

    let overflows = Arc::new(AtomicU64::new(0));
    let callbacks = Arc::new(AtomicU64::new(0));
    let rate_est = Arc::new(AtomicU64::new((cfg.sample_rate as f64).to_bits()));

    let build = |conf: &cpal::StreamConfig| -> anyhow::Result<(cpal::Stream, AudioFeed)> {
        let ch = conf.channels as usize;
        let (prod, cons) = HeapRb::<f32>::new(ch * cfg.sample_rate as usize / 2).split();
        let (tprod, tcons) = HeapRb::<(u64, i64)>::new(4096).split();
        let st = match sc.sample_format() {
            SampleFormat::F32 => build_in::<f32>(&dev, conf, prod, tprod, overflows.clone(), callbacks.clone()),
            SampleFormat::I32 => build_in::<i32>(&dev, conf, prod, tprod, overflows.clone(), callbacks.clone()),
            SampleFormat::I24 => build_in::<cpal::I24>(&dev, conf, prod, tprod, overflows.clone(), callbacks.clone()),
            SampleFormat::I16 => build_in::<i16>(&dev, conf, prod, tprod, overflows.clone(), callbacks.clone()),
            f => return Err(anyhow!("Sampleformat {f} nicht unterstützt")),
        }?;
        Ok((st, AudioFeed { cons, timing: tcons, channels: ch, rate_est: rate_est.clone() }))
    };
    let (stream, feed) = match build(&conf) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Eingang mit Puffer {} / {} Kanälen fehlgeschlagen ({e}) – Treiber-Defaults", cfg.buffer_frames, conf.channels);
            conf = sc.config();
            build(&conf)?
        }
    };
    stream.play().context("Eingangs-Stream starten")?;
    log::info!("Audio-Eingang: \"{dev}\" {} Kanäle @ {} Hz, {:?}, Puffer {:?}", conf.channels, conf.sample_rate, sc.sample_format(), conf.buffer_size);
    let stats = AudioStats {
        device: dev.to_string(),
        channels: conf.channels as usize,
        sample_rate: conf.sample_rate,
        overflows,
        underruns: Arc::new(AtomicU64::new(0)),
        rate_est,
        callbacks,
    };
    Ok((InputGuard::Device(stream), feed, stats))
}

fn test_tone(cfg: &Config) -> (InputGuard, AudioFeed, AudioStats) {
    let ch = if cfg.input_channels == 0 { 16 } else { cfg.input_channels as usize };
    let rate = cfg.sample_rate;
    let (mut prod, cons) = HeapRb::<f32>::new(ch * rate as usize / 2).split();
    let (mut tprod, tcons) = HeapRb::<(u64, i64)>::new(4096).split();
    let callbacks = Arc::new(AtomicU64::new(0));
    let overflows = Arc::new(AtomicU64::new(0));
    let rate_est = Arc::new(AtomicU64::new((rate as f64).to_bits()));
    let (cb, ov) = (callbacks.clone(), overflows.clone());
    let block = 64usize;
    std::thread::Builder::new()
        .name("test-tone".into())
        .spawn(move || {
            let start = now_ns();
            let mut written: u64 = 0;
            let mut buf = vec![0f32; block * ch];
            loop {
                let due = start + ((written + block as u64) as f64 * 1e9 / rate as f64) as i64;
                let now = now_ns();
                if now < due {
                    std::thread::sleep(std::time::Duration::from_nanos((due - now) as u64));
                    continue;
                }
                for f in 0..block {
                    let n = written + f as u64;
                    let s = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / rate as f64).sin() as f32 * 0.1;
                    let pos = n % rate as u64;
                    for c in 0..ch {
                        let click = (pos % (rate as u64 / (c as u64 + 1)) < 48) && pos < rate as u64 / 2;
                        buf[f * ch + c] = if click { 0.5 } else { s };
                    }
                }
                cb.fetch_add(1, Ordering::Relaxed);
                if prod.vacant_len() < buf.len() {
                    ov.fetch_add(1, Ordering::Relaxed);
                } else {
                    prod.push_slice(&buf);
                    written += block as u64;
                    let _ = tprod.try_push((written, now_ns()));
                }
            }
        })
        .expect("Testton-Thread");
    log::info!("Audio-Eingang: Testton-Generator, {ch} Kanäle @ {rate} Hz");
    let stats = AudioStats {
        device: "Testton".into(),
        channels: ch,
        sample_rate: rate,
        overflows,
        underruns: Arc::new(AtomicU64::new(0)),
        rate_est: rate_est.clone(),
        callbacks,
    };
    (InputGuard::TestTone, AudioFeed { cons, timing: tcons, channels: ch, rate_est }, stats)
}

fn build_in<T>(
    dev: &cpal::Device,
    conf: &cpal::StreamConfig,
    mut prod: ringbuf::HeapProd<f32>,
    mut tprod: ringbuf::HeapProd<(u64, i64)>,
    overflows: Arc<AtomicU64>,
    callbacks: Arc<AtomicU64>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let ch = conf.channels as usize;
    let mut scratch: Vec<f32> = Vec::with_capacity(ch * 8192);
    let mut written: u64 = 0;
    Ok(dev.build_input_stream::<T, _, _>(
        conf.clone(),
        move |data: &[T], _| {
            let t = now_ns();
            callbacks.fetch_add(1, Ordering::Relaxed);
            scratch.clear();
            scratch.extend(data.iter().map(|s| f32::from_sample_(*s)));
            if prod.vacant_len() < scratch.len() {
                // Ganzen Block verwerfen, damit die Frame-Zählung konsistent bleibt
                overflows.fetch_add(1, Ordering::Relaxed);
                return;
            }
            prod.push_slice(&scratch);
            written += (scratch.len() / ch) as u64;
            let _ = tprod.try_push((written, t));
        },
        |e| log::error!("Audio-Eingang: {e}"),
        None,
    )?)
}

// ------------------------------------------------------------------ Backend "device"

/// Gibt das geroutete Signal auf einem OS-Audio-Device aus (z.B. dem nativen
/// AVB-Device von macOS). Da beide Devices unterschiedliche Clocks haben,
/// gleicht ein adaptiver Resampler (Catmull-Rom, PI-geregelt) die Drift aus.
pub fn open_output(cfg: &Config, feed: AudioFeed, router: Arc<Router>, underruns: Arc<AtomicU64>) -> anyhow::Result<cpal::Stream> {
    let host = host(&cfg.audio_host)?;
    let dev = find_device(&host, &cfg.output_device, false)?;
    let sc = pick_config(dev.supported_output_configs()?, cfg.sample_rate, cfg.output_channels)?;
    let conf = stream_config(&sc, cfg.output_channels, cfg.buffer_frames);
    let st = match sc.sample_format() {
        SampleFormat::F32 => build_out::<f32>(&dev, &conf, feed, router, cfg, underruns),
        SampleFormat::I32 => build_out::<i32>(&dev, &conf, feed, router, cfg, underruns),
        SampleFormat::I24 => build_out::<cpal::I24>(&dev, &conf, feed, router, cfg, underruns),
        SampleFormat::I16 => build_out::<i16>(&dev, &conf, feed, router, cfg, underruns),
        f => return Err(anyhow!("Sampleformat {f} nicht unterstützt")),
    }?;
    st.play()?;
    log::info!("Audio-Ausgang: \"{dev}\" {} Kanäle @ {} Hz", conf.channels, conf.sample_rate);
    Ok(st)
}

pub struct DriftResampler {
    ch: usize,
    hist: Vec<f32>, // 4 Frames: x[-1], x[0], x[1], x[2]
    frac: f64,
    ratio: f64,
    integ: f64,
    fill_avg: f64,
    target: f64,
    primed: bool,
    tmp: Vec<f32>,
}

impl DriftResampler {
    pub fn new(ch: usize, target_frames: f64) -> Self {
        Self { ch, hist: vec![0.0; 4 * ch], frac: 0.0, ratio: 1.0, integ: 0.0, fill_avg: target_frames, target: target_frames, primed: false, tmp: vec![0.0; ch] }
    }

    /// Einmal pro Callback: Füllstand -> Ratio (PI-Regler, max ±1000 ppm).
    pub fn control(&mut self, fill_frames: f64, dt_s: f64) {
        self.fill_avg += (fill_frames - self.fill_avg) * 0.05;
        let err = (self.fill_avg - self.target) / self.target.max(1.0);
        self.integ = (self.integ + err * dt_s).clamp(-5.0, 5.0);
        self.ratio = 1.0 + (2e-4 * err + 2e-5 * self.integ).clamp(-1e-3, 1e-3);
    }

    /// Erzeugt einen Ausgangs-Frame; `pull` liefert den nächsten Eingangs-Frame.
    #[inline]
    pub fn next_frame(&mut self, cons: &mut HeapCons<f32>, underruns: &AtomicU64) {
        self.frac += self.ratio;
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            self.hist.copy_within(self.ch.., 0);
            let last = 3 * self.ch;
            if self.primed && cons.pop_slice(&mut self.tmp) == self.ch {
                self.hist[last..].copy_from_slice(&self.tmp);
            } else {
                if self.primed {
                    underruns.fetch_add(1, Ordering::Relaxed);
                    self.primed = false;
                }
                self.hist[last..].fill(0.0);
            }
        }
    }

    #[inline]
    pub fn sample(&self, c: usize) -> f32 {
        let ch = self.ch;
        let (y0, y1, y2, y3) = (self.hist[c], self.hist[ch + c], self.hist[2 * ch + c], self.hist[3 * ch + c]);
        let t = self.frac as f32;
        let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
        let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let cc = -0.5 * y0 + 0.5 * y2;
        ((a * t + b) * t + cc) * t + y1
    }
}

fn build_out<T>(
    dev: &cpal::Device,
    conf: &cpal::StreamConfig,
    mut feed: AudioFeed,
    router: Arc<Router>,
    cfg: &Config,
    underruns: Arc<AtomicU64>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let out_ch = conf.channels as usize;
    let in_ch = feed.channels;
    let rate = conf.sample_rate as f64;
    let target = (cfg.device_latency_ms as f64 / 1000.0 * rate).max(64.0);
    let mut rs = DriftResampler::new(in_ch, target);
    let n_routed = router.num_outputs().min(out_ch);
    Ok(dev.build_output_stream::<T, _, _>(
        conf.clone(),
        move |data: &mut [T], _| {
            while feed.timing.try_pop().is_some() {}
            let fill = (feed.cons.occupied_len() / in_ch) as f64;
            if !rs.primed && fill >= target {
                rs.primed = true;
            }
            let frames = data.len() / out_ch;
            rs.control(fill, frames as f64 / rate);
            feed.rate_est.store((rate * rs.ratio).to_bits(), Ordering::Relaxed);
            for f in 0..frames {
                rs.next_frame(&mut feed.cons, &underruns);
                let o = &mut data[f * out_ch..(f + 1) * out_ch];
                for (c, s) in o.iter_mut().enumerate() {
                    let v = if c < n_routed { router.source(c).map_or(0.0, |src| rs.sample(src)) } else { 0.0 };
                    *s = T::from_sample_(v);
                }
            }
        },
        |e| log::error!("Audio-Ausgang: {e}"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampler_passes_signal_at_unity_ratio() {
        let (mut p, mut c) = HeapRb::<f32>::new(1024).split();
        for i in 0..512 {
            p.try_push(i as f32).unwrap();
        }
        let u = AtomicU64::new(0);
        let mut r = DriftResampler::new(1, 10.0);
        r.primed = true;
        let mut out = vec![];
        for _ in 0..100 {
            r.next_frame(&mut c, &u);
            out.push(r.sample(0));
        }
        // Verzögerung um 2 Frames, sonst identisch
        for (i, v) in out.iter().enumerate().skip(3) {
            assert!((v - (i as f32 - 2.0)).abs() < 1e-3, "{i} {v}");
        }
    }
}
