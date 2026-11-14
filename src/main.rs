//! Dante <-> AVB/Milan Bridge
//!
//! Dante-Audio kommt über die Dante Virtual Soundcard (CoreAudio/ASIO/WASAPI),
//! wird über eine frei konfigurierbare Matrix geroutet und als Milan-konformer
//! AVB-Talker (AAF 48 kHz / 8 Kanäle pro Stream) ins AVB-Netz gesendet.

mod audio;
mod avb;
mod config;
#[cfg(target_os = "macos")]
mod macos_mac;
mod net;
mod routing;
mod timing;
mod web;

use config::{Backend, Config};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn usage() {
    eprintln!(
        "dante-milan-bridge {}\n\n\
         Befehle:\n  \
         init [config.json]       Standard-Konfiguration schreiben\n  \
         list-audio               Audio-Devices auflisten (Dante Virtual Soundcard finden)\n  \
         list-interfaces          Netzwerkschnittstellen auflisten (AVB-Port finden)\n  \
         run [config.json]        Bridge starten (Standard: bridge.json)\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("run");
    let cfg_path = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("bridge.json"));
    match cmd {
        "init" => {
            Config::default().save(&cfg_path)?;
            println!("{} geschrieben", cfg_path.display());
        }
        "list-audio" => audio::list_audio()?,
        "list-interfaces" => {
            for i in net::list_interfaces()? {
                println!(
                    "{:<45} MAC {}  \"{}\"",
                    i.pcap_name,
                    i.mac.map(|m| net::fmt_mac(&m)).unwrap_or_else(|| "-".into()),
                    i.label
                );
            }
        }
        "run" => run(cfg_path)?,
        _ => usage(),
    }
    Ok(())
}

fn run(cfg_path: PathBuf) -> anyhow::Result<()> {
    let cfg = Config::load(&cfg_path)?;
    let (_in_stream, feed, stats) = audio::open_input(&cfg)?;
    let router = Arc::new(routing::Router::new(stats.channels, cfg.num_outputs(), &cfg.routing));

    let mut _out_stream = None;
    let stack = match cfg.backend {
        Backend::Milan => {
            let netif = net::resolve_interface(&cfg.avb_interface)?;
            let stack = avb::stack::AvbStack::start(&netif, &cfg)?;
            if cfg.tx_delay_us * 48 / 1000 < cfg.buffer_frames.max(64) * 2 {
                log::warn!("tx_delay_us ({}) ist knapp im Verhältnis zur Audio-Puffergröße – ggf. erhöhen", cfg.tx_delay_us);
            }
            avb::talker::spawn(stack.clone(), feed, router.clone(), stats.sample_rate, cfg.tx_delay_us, cfg.presentation_offset_us)?;
            Some(stack)
        }
        Backend::Device => {
            _out_stream = Some(audio::open_output(&cfg, feed, router.clone(), stats.underruns.clone())?);
            None
        }
    };

    let listen = cfg.web_listen.clone();
    let app = Arc::new(web::App { cfg: parking_lot::Mutex::new(cfg), cfg_path, router, stack: stack.clone(), audio: stats });

    {
        let app = app.clone();
        ctrlc::set_handler(move || {
            log::info!("Beende …");
            if let Some(s) = &app.stack {
                s.shutdown();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        })?;
    }
    // Von einem Controller (Hive/Network Manager) geänderte Namen automatisch sichern
    if let Some(s) = stack {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if s.entity.lock().names_dirty && s.running.load(Ordering::Relaxed) {
                let _ = app.save();
            }
        });
    }
    web::serve(app, &listen)
}
