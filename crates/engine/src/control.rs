//! Local control channel: JSON lines over TCP on 127.0.0.1 (off by default).
//!
//! Every request is a JSON object per line: `{"id": 1, "method": "command", "params": {"name":
//! "record.toggle"}}`; every reply is `{"id": 1, "result": …}` or `{"id": 1, "error": "…"}`.
//! Methods: `state`, `command` (`name`, optional `arg`), `settings`, `recoveries`, `log`.
//! Requests are executed on the engine's thread (in [`crate::Engine::tick`]).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use serde_json::{Value, json};

use crate::{Command, Engine};

pub struct Pending {
    pub request: Value,
    reply: Sender<Value>,
}

impl Pending {
    pub fn respond(self, v: Value) {
        let _ = self.reply.send(v);
    }
}

pub struct ControlServer {
    rx: Receiver<Pending>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    port: u16,
}

impl ControlServer {
    pub fn start(port: u16) -> Result<ControlServer, String> {
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("control channel on port {port}: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let (tx, rx) = bounded(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fluxcapture-control".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((s, _)) => {
                            let tx = tx.clone();
                            let _ = std::thread::Builder::new().name("fluxcapture-control-conn".into()).spawn(move || {
                                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(s, &tx)));
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        log::info!("control channel listening on 127.0.0.1:{port}");
        Ok(ControlServer { rx, stop, thread: Some(thread), port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn drain(&self) -> Vec<Pending> {
        self.rx.try_iter().take(32).collect()
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(stream: TcpStream, tx: &Sender<Pending>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let Ok(mut out) = stream.try_clone() else { return };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.len() > 1 << 20 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => {
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let (rtx, rrx) = bounded(1);
                if tx.send_timeout(Pending { request: req, reply: rtx }, Duration::from_secs(5)).is_err() {
                    json!({"id": id, "error": "the engine is busy"})
                } else {
                    match rrx.recv_timeout(Duration::from_secs(30)) {
                        Ok(mut v) => {
                            if let Some(o) = v.as_object_mut() {
                                o.insert("id".into(), id);
                            }
                            v
                        }
                        Err(_) => json!({"id": id, "error": "timeout"}),
                    }
                }
            }
            Err(e) => json!({"id": null, "error": format!("invalid JSON: {e}")}),
        };
        if writeln!(out, "{reply}").is_err() {
            break;
        }
    }
}

/// Executes one request against the engine.
pub fn handle(engine: &mut Engine, req: &Value) -> Value {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "state" => json!({"result": engine.status_json()}),
        "settings" => match serde_json::to_value(engine.settings()) {
            Ok(v) => json!({"result": v}),
            Err(e) => json!({"error": e.to_string()}),
        },
        "recoveries" => json!({"result": engine.pending_recoveries().iter().map(|(_, e)| e.files.clone()).collect::<Vec<_>>()}),
        "log" => json!({"result": engine.log().lines().iter().rev().take(200).map(|l| l.text.clone()).collect::<Vec<_>>()}),
        "command" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arg = params.get("arg").and_then(Value::as_u64);
            match Command::from_id(name, arg) {
                Some(cmd) => match engine.dispatch(cmd) {
                    Ok(()) => json!({"result": "ok"}),
                    Err(e) => json!({"error": e}),
                },
                None => json!({"error": format!("unknown command {name:?}")}),
            }
        }
        other => json!({"error": format!("unknown method {other:?}")}),
    }
}
