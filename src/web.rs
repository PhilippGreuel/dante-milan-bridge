//! Lokale Web-Oberfläche: Routing-Matrix, Kanalnamen, Status.

use crate::audio::AudioStats;
use crate::avb::stack::AvbStack;
use crate::config::{Backend, Config};
use crate::net;
use crate::routing::Router;
use crate::timing::now_ns;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct App {
    pub cfg: Mutex<Config>,
    pub cfg_path: PathBuf,
    pub router: Arc<Router>,
    pub stack: Option<Arc<AvbStack>>,
    pub audio: AudioStats,
}

const INDEX: &str = include_str!("../ui/index.html");

impl App {
    fn names(list: &[String], n: usize, prefix: &str) -> Vec<String> {
        (0..n).map(|i| list.get(i).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("{prefix} {}", i + 1))).collect()
    }

    pub fn state(&self) -> Value {
        let cfg = self.cfg.lock();
        let inputs = Self::names(&cfg.input_names, self.router.num_inputs(), "Dante");
        let mut outputs = Self::names(&cfg.output_names, self.router.num_outputs(), "AVB");
        let avb = self.stack.as_ref().map(|s| {
            let e = s.entity.lock();
            outputs = e.cluster_names.clone();
            let g = s.gptp.lock();
            let maap = s.maap.lock();
            json!({
                "mac": net::fmt_mac(&s.mac),
                "entity_id": format!("0x{:016X}", e.entity_id),
                "entity_name": e.entity_name,
                "gptp_locked": g.is_locked(now_ns()),
                "gm_id": format!("0x{:016X}", g.gm_id),
                "link_delay_ns": g.link_delay_ns,
                "link_delay_jitter_ns": g.delay_jitter_ns,
                "turnaround_comp_ns": g.turnaround_comp_ns,
                "neighbor": s.neighbor.lock().target.map(|t| format!("0x{:016X}", t.0)),
                "neighbor_reported_ns": s.neighbor.lock().reported.map(|r| r.0),
                "tx_echo_hits": g.tx_echo_hits,
                "tx_echo_misses": g.tx_echo_misses,
                "rate_ppm": g.rate_ppm(),
                "offset_ns": g.offset_ns(),
                "syncs": g.syncs,
                "pdelay_responses": g.pdelay_responses_sent,
                "vlan": s.vid.load(Ordering::Relaxed),
                "maap_acquired": maap.acquired(),
                "tx_packets": s.tx.sent.load(Ordering::Relaxed),
                "tx_errors": s.tx.errors.load(Ordering::Relaxed),
                "rx_packets": s.rx_packets.load(Ordering::Relaxed),
                "streams": s.streams.iter().enumerate().map(|(i, st)| json!({
                    "index": i,
                    "name": e.stream_names[i],
                    "stream_id": format!("0x{:016X}", st.stream_id),
                    "dest": net::fmt_mac(&st.dest()),
                    "listener": match st.listener.load(Ordering::Relaxed) { 2 => "ready", 3 => "ready_failed", 1 => "asking_failed", _ => "none" },
                    "streaming": st.streaming.load(Ordering::Relaxed),
                    "packets": st.packets.load(Ordering::Relaxed),
                    "late": st.late.load(Ordering::Relaxed),
                    "dropped": st.dropped.load(Ordering::Relaxed),
                    "probes": st.probes.load(Ordering::Relaxed),
                })).collect::<Vec<_>>(),
            })
        });
        json!({
            "backend": if cfg.backend == Backend::Milan { "milan" } else { "device" },
            "inputs": inputs,
            "outputs": outputs,
            "routing": self.router.snapshot(),
            "audio": {
                "device": self.audio.device,
                "channels": self.audio.channels,
                "sample_rate": self.audio.sample_rate,
                "rate_est": f64::from_bits(self.audio.rate_est.load(Ordering::Relaxed)),
                "overflows": self.audio.overflows.load(Ordering::Relaxed),
                "underruns": self.audio.underruns.load(Ordering::Relaxed),
                "callbacks": self.audio.callbacks.load(Ordering::Relaxed),
            },
            "avb": avb,
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let mut cfg = self.cfg.lock();
        cfg.routing = self.router.snapshot();
        if let Some(s) = &self.stack {
            let mut e = s.entity.lock();
            cfg.entity_name = e.entity_name.clone();
            cfg.group_name = e.group_name.clone();
            cfg.output_names = e.cluster_names.clone();
            e.names_dirty = false;
        }
        cfg.save(&self.cfg_path)?;
        log::info!("Konfiguration gespeichert: {}", self.cfg_path.display());
        Ok(())
    }

    fn api(&self, method: &str, path: &str, body: &str) -> Result<Value, String> {
        let v: Value = if body.is_empty() { Value::Null } else { serde_json::from_str(body).map_err(|e| e.to_string())? };
        match (method, path) {
            ("GET", "/api/state") => Ok(self.state()),
            ("POST", "/api/route") => {
                let out = v["out"].as_u64().ok_or("out fehlt")? as usize;
                let src = v["src"].as_u64().map(|s| s as usize);
                if self.router.set(out, src) { Ok(json!({"ok": true})) } else { Err("ungültiger Kanal".into()) }
            }
            ("POST", "/api/routing") => {
                let arr = v.as_array().ok_or("Array erwartet")?;
                for (o, s) in arr.iter().enumerate() {
                    self.router.set(o, s.as_u64().map(|x| x as usize));
                }
                Ok(json!({"ok": true}))
            }
            ("POST", "/api/name") => {
                let idx = v["index"].as_u64().ok_or("index fehlt")? as usize;
                let name = v["name"].as_str().ok_or("name fehlt")?.chars().take(63).collect::<String>();
                match v["kind"].as_str() {
                    Some("input") => {
                        let mut c = self.cfg.lock();
                        if c.input_names.len() <= idx {
                            c.input_names.resize(idx + 1, String::new());
                        }
                        c.input_names[idx] = name;
                    }
                    Some("output") => {
                        if let Some(s) = &self.stack {
                            if let Some(n) = s.entity.lock().cluster_names.get_mut(idx) {
                                *n = name.clone();
                            }
                        }
                        let mut c = self.cfg.lock();
                        if c.output_names.len() <= idx {
                            c.output_names.resize(idx + 1, String::new());
                        }
                        c.output_names[idx] = name;
                    }
                    _ => return Err("kind = input|output".into()),
                }
                Ok(json!({"ok": true}))
            }
            ("POST", "/api/save") => self.save().map(|_| json!({"ok": true})).map_err(|e| e.to_string()),
            _ => Err("not found".into()),
        }
    }
}

pub fn serve(app: Arc<App>, listen: &str) -> anyhow::Result<()> {
    let server = tiny_http::Server::http(listen).map_err(|e| anyhow::anyhow!("Web-UI {listen}: {e}"))?;
    log::info!("Web-Oberfläche: http://{listen}/");
    let json_hdr = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
    let html_hdr = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
    for mut req in server.incoming_requests() {
        let method = req.method().to_string();
        let path = req.url().split('?').next().unwrap_or("/").to_string();
        let resp = if path == "/" {
            tiny_http::Response::from_string(INDEX).with_header(html_hdr.clone())
        } else {
            let mut body = String::new();
            let _ = req.as_reader().take(1 << 20).read_to_string(&mut body);
            match app.api(&method, &path, &body) {
                Ok(v) => tiny_http::Response::from_string(v.to_string()).with_header(json_hdr.clone()),
                Err(e) => tiny_http::Response::from_string(json!({"error": e}).to_string()).with_status_code(400).with_header(json_hdr.clone()),
            }
        };
        let _ = req.respond(resp);
    }
    Ok(())
}
