//! Persistente Konfiguration (JSON).

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Eigener AVB/Milan-Stack über pcap (Windows: Npcap, macOS/Linux: libpcap).
    Milan,
    /// Ausgabe auf ein Audio-Device des Betriebssystems (z.B. das native
    /// AVB-Audio-Device von macOS). gPTP/MSRP/AVDECC übernimmt dann das OS.
    Device,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Teil des Namens des Eingangs-Devices, z.B. "Dante Virtual Soundcard".
    pub dante_device: String,
    /// Audio-Host: "default" oder (Windows mit Feature `asio`) "asio".
    pub audio_host: String,
    /// Puffergröße in Frames (0 = Default des Treibers).
    pub buffer_frames: u32,
    pub sample_rate: u32,
    /// Anzahl Eingangskanäle, die vom Dante-Device geöffnet werden (0 = max).
    pub input_channels: u16,

    pub backend: Backend,

    // ---- Backend "milan" ----
    /// Netzwerkschnittstelle zum AVB-Netz (Name aus `list-interfaces`).
    pub avb_interface: String,
    pub entity_name: String,
    pub group_name: String,
    /// Anzahl AVB-Talker-Streams (je 8 Kanäle). 2 => 16 Kanäle wie AVB-D16.
    pub avb_streams: u16,
    /// Max Transit Time (Milan Default 2 ms), wird auf die Sendezeit addiert.
    pub presentation_offset_us: u32,
    /// Verzögerung zwischen Audio-Capture und Versand (muss > Puffergröße sein).
    pub tx_delay_us: u32,
    /// Streams auch ohne registrierten Listener senden (nur Test).
    pub always_stream: bool,

    // ---- Backend "device" ----
    pub output_device: String,
    pub output_channels: u16,
    /// Ziel-Füllstand des Drift-Ausgleichspuffers in ms.
    pub device_latency_ms: f32,

    // ---- Routing / UI ----
    /// routing[ausgang] = Index des Dante-Eingangskanals (oder null = stumm).
    pub routing: Vec<Option<u16>>,
    pub input_names: Vec<String>,
    pub output_names: Vec<String>,
    pub web_listen: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dante_device: "Dante Virtual Soundcard".into(),
            audio_host: "default".into(),
            buffer_frames: 64,
            sample_rate: 48000,
            input_channels: 16,
            backend: Backend::Milan,
            avb_interface: String::new(),
            entity_name: "Dante-Milan Bridge".into(),
            group_name: String::new(),
            avb_streams: 2,
            presentation_offset_us: 2000,
            tx_delay_us: 4000,
            always_stream: false,
            output_device: String::new(),
            output_channels: 16,
            device_latency_ms: 5.0,
            routing: Vec::new(),
            input_names: Vec::new(),
            output_names: Vec::new(),
            web_listen: "127.0.0.1:8090".into(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            log::warn!("{} existiert nicht – verwende Standardwerte", path.display());
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&s)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn num_outputs(&self) -> usize {
        match self.backend {
            Backend::Milan => self.avb_streams as usize * crate::avb::CHANNELS_PER_STREAM,
            Backend::Device => self.output_channels as usize,
        }
    }
}
