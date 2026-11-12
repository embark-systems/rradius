//! `calibrate`: what the workers of `run` can do on this machine, against responders in
//! this process that do only what a response takes.
use crate::cli::{Kind, LoadSpec};
use crate::packet::{write_response, RequestOptions, Signer, Step, ACCESS_ACCEPT, MAX_PACKET};
use crate::report::Latency;
use crate::run::{run_loads, RunParams};
use crate::worker::Targets;
use serde::Serialize;
use std::fmt::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const SECRET: &[u8] = b"calibrate";

#[derive(Debug, Serialize)]
pub struct Calibration {
    pub schema: u32,
    pub results: Vec<CalibrationResult>,
}

#[derive(Debug, Serialize)]
pub struct CalibrationResult {
    pub threads: u32,
    pub window: u32,
    pub concurrency: u32,
    pub throughput_per_s: f64,
    pub latency_us: Latency,
    pub worker_busy_max: f64,
    pub timeouts: u64,
}

/// Answers on its own thread until told to stop.
struct Responder {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Responder {
    fn start(signer: Signer) -> Result<Responder, String> {
        let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("responder: {e}"))?;
        socket.set_read_timeout(Some(Duration::from_millis(50))).map_err(|e| format!("responder: {e}"))?;
        let address = socket.local_addr().map_err(|e| format!("responder: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut request = [0u8; MAX_PACKET];
            let mut response = [0u8; MAX_PACKET];
            while !stopping.load(Ordering::Relaxed) {
                if let Ok((length, from)) = socket.recv_from(&mut request) {
                    let size = write_response(&mut response, &request[..length], ACCESS_ACCEPT, &[], true, &signer);
                    let _ = socket.send_to(&response[..size], from);
                }
            }
        });
        Ok(Responder { address, stop, handle })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn measure(threads: u32, window: u32, concurrency: u32, duration: Duration) -> Result<CalibrationResult, String> {
    let signer = Signer::new(SECRET)?;
    let spec = LoadSpec { kind: Kind::Single(Step::Pap), concurrency, rate: None, window, requested_threads: threads, alive: 0 };
    let responders = (0..threads).map(|_| Responder::start(signer.clone())).collect::<Result<Vec<_>, _>>()?;
    let addresses: Vec<SocketAddr> = responders.iter().map(|responder| responder.address).collect();
    let params = RunParams {
        signer,
        options: RequestOptions { nas_ip: "192.0.2.1".parse().map_err(|_| "address")?, nas_identifier: "calibrate".to_string(), extra: Vec::new(), message_authenticator: true },
        user: "bench".to_string(),
        password: "bench".to_string(),
        users: None,
        bind: None,
        warmup: Duration::ZERO,
        duration: Some(duration),
        requests: None,
        timeout: Duration::from_secs(1),
        retries: 0,
        eap: crate::eap::EapOptions::default(),
        radsec: None,
        tacacs: None,
    };
    let targets = |_: usize, thread: u32| {
        let address = addresses[thread as usize];
        Targets { auth: address, acct: address }
    };
    let result = run_loads(std::slice::from_ref(&spec), &params, targets, || {});
    for responder in responders {
        responder.stop();
    }
    let outcomes = result?;
    let outcome = outcomes.first().ok_or("no result")?;
    let (_, stats) = outcome.steps.first().ok_or("no result")?;
    Ok(CalibrationResult {
        threads,
        window,
        concurrency,
        throughput_per_s: stats.responses() as f64 / outcome.window.as_secs_f64(),
        latency_us: Latency::from_histogram(&stats.latency),
        worker_busy_max: outcome.busy_max,
        timeouts: stats.timeouts,
    })
}

/// One request at a time on one thread, then a window of 64 per socket on 1, 2 and 4 threads.
pub fn calibrate(duration: f64) -> Result<Calibration, String> {
    let duration = Duration::try_from_secs_f64(duration).map_err(|_| format!("--duration {duration}: not a time in seconds"))?;
    if duration.is_zero() {
        return Err("--duration must be above 0".to_string());
    }
    let mut results = vec![measure(1, 1, 1, duration)?];
    for threads in [1, 2, 4] {
        results.push(measure(threads, 64, threads * 64, duration)?);
    }
    Ok(Calibration { schema: 1, results })
}

impl Calibration {
    pub fn to_text(&self) -> String {
        let mut text = String::from("what this tool can do here, against responders that do no other work:\n");
        for result in &self.results {
            let _ = writeln!(
                text,
                "  {} threads, window {}, concurrency {}: {:.0}/s, p50 {:.1} us, p99 {:.1} us, busiest worker {:.0}%, timeouts {}",
                result.threads,
                result.window,
                result.concurrency,
                result.throughput_per_s,
                result.latency_us.p50,
                result.latency_us.p99,
                result.worker_busy_max * 100.0,
                result.timeouts
            );
        }
        text
    }
}
