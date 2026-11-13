//! `run` against scripted responders: what happens on the wire, in the report and in the exit code.
mod common;

use common::*;
use rradius_bench::packet::{ACCESS_ACCEPT, ACCESS_CHALLENGE, CLASS, FRAMED_IP_ADDRESS};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USER_NAME: u8 = 1;
const ACCT_SESSION_ID: u8 = 44;
const ACCT_STATUS_TYPE: u8 = 40;

#[test]
fn test_invalid_arguments_exit_3_and_send_nothing() {
    let responder = Responder::answering();
    for args in [
        &["--load", "pap"][..],
        &["--load", "pap,concurrency=1,window=257"],
        &["--load", "pap,concurrency=1,alive=1"],
        &["--load", "pap,concurrency=1", "--attr", "80=hex:00"],
        &["--load", "pap,concurrency=1", "--requests", "0"],
        &["--load", "pap,concurrency=1", "--users", "0"],
        &["--load", "pap,concurrency=1", "--timeout", "0"],
        &["--load", "pap,concurrency=1", "--metrics-url", "https://127.0.0.1:1/metrics"],
        &["--load", "pap,concurrency=1", "--requests", "1", "--duration", "1"],
        &["--load", "pap,concurrency=1", "--requests", "1", "--warmup", "1"],
        &["--load", "eap-tls,concurrency=1"],
        &["--load", "eap-tls,concurrency=1", "--cert-file", "/nonexistent.pem", "--key-file", "/nonexistent.key"],
        &["--load", "peap,concurrency=1", "--tls-version", "1.3"],
        &["--load", "ttls,concurrency=1", "--tls-version", "1.3"],
        &["--load", "peap,concurrency=1", "--tls-version", "1.1"],
        &["--load", "peap,concurrency=1", "--no-message-authenticator"],
        &["--load", "ttls,concurrency=1", "--eap-fragment", "10"],
        &["--load", "peap,concurrency=1", "--ca-file", "/nonexistent.pem"],
    ] {
        let output = run_against(responder.address, args);
        assert_eq!(output.code, 3, "{args:?}: {}", output.stderr);
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    assert_eq!(bench(&["run".to_string()]).code, 3);
    std::thread::sleep(Duration::from_millis(100));
    assert!(responder.received.lock().unwrap().is_empty());
}

/// An EAP argument that cannot work is named, so the reader knows what to change.
#[test]
fn test_eap_arguments_are_named() {
    let responder = Responder::answering();
    for (args, named) in [
        (&["--load", "eap-tls,concurrency=1"][..], "--cert-file"),
        (&["--load", "eap-tls,concurrency=1", "--cert-file", "/nonexistent.pem", "--key-file", "/nonexistent.key"], "--cert-file"),
        (&["--load", "peap,concurrency=1", "--tls-version", "1.3"], "--tls-version"),
        (&["--load", "peap,concurrency=1", "--no-message-authenticator"], "--no-message-authenticator"),
        (&["--load", "ttls,concurrency=1", "--eap-fragment", "10"], "--eap-fragment"),
        (&["--load", "peap,concurrency=1", "--ca-file", "/nonexistent.pem"], "--ca-file"),
        (&["--load", "pap,concurrency=1", "--transport", "tls"], "--cert-file"),
        (&["--load", "pap,concurrency=1", "--transport", "tls", "--cert-file", "c.pem", "--key-file", "c.key", "--retries", "1"], "--retries"),
        (&["--load", "pap,concurrency=1", "--transport", "tls", "--cert-file", "c.pem", "--key-file", "c.key", "--tls-port", "0"], "--tls-port"),
        (&["--load", "pap,concurrency=1", "--transport", "dtls"], "--transport"),
        (&["--load", "pap,concurrency=1", "--transport", "tls", "--cert-file", "/nonexistent.pem", "--key-file", "/nonexistent.key"], "--cert-file"),
    ] {
        let output = run_against(responder.address, args);
        assert_eq!(output.code, 3, "{args:?}: {}", output.stderr);
        assert!(output.stderr.contains(named), "{args:?}: {}", output.stderr);
    }
}

/// TACACS+ arguments that cannot work are named, and nothing reaches either port.
#[test]
fn test_tacacs_arguments_refused() -> Result<(), String> {
    let responder = Responder::answering();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port().to_string();
    for (args, named) in [
        (&["--load", "tacacs-pap,concurrency=4,window=2"][..], "window"),
        (&["--load", "tacacs-acct,concurrency=1", "--transport", "tls", "--cert-file", "c.pem", "--key-file", "c.key"], "--transport"),
        (&["--load", "tacacs-pap,concurrency=1", "--tacacs-port", "0"], "--tacacs-port"),
        (&["--load", "tacacs-pap,concurrency=1", "--tacacs-connect", "sometimes"], "--tacacs-connect"),
    ] {
        let mut all = args.to_vec();
        if !args.contains(&"--tacacs-port") {
            all.extend(["--tacacs-port", &port]);
        }
        let output = run_against(responder.address, &all);
        if output.code != 3 || !output.stderr.contains(named) {
            return Err(format!("{args:?}: exit {} / {}", output.code, output.stderr));
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    if listener.accept().is_ok() {
        return Err("a TACACS+ connection was opened".to_string());
    }
    let received = responder.received.lock().map_err(|e| e.to_string())?;
    if !received.is_empty() {
        return Err("a RADIUS request was sent".to_string());
    }
    Ok(())
}

#[test]
fn test_help_exits_0() {
    assert_eq!(bench(&["--help".to_string()]).code, 0);
    assert_eq!(bench(&["run".to_string(), "--help".to_string()]).code, 0);
}

#[test]
fn test_unresolvable_server_exits_3() {
    let args: Vec<String> = ["run", "--server", "no-such-host.invalid", "--secret", "s", "--load", "pap,concurrency=1", "--requests", "1"].iter().map(|s| s.to_string()).collect();
    assert_eq!(bench(&args).code, 3);
}

#[test]
fn test_one_request() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1"]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    assert_eq!(step(&json, 0, 0)["sent"], 1);
    assert_eq!(step(&json, 0, 0)["responses"]["accept"], 1);
    assert_eq!(responder.requests().len(), 1);
    assert_eq!(json["warmup_s"], 0.0);
}

#[test]
fn test_json_output() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "acct-start,concurrency=4,window=2", "--warmup", "0", "--duration", "0.2"]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    assert_eq!(json["schema"], 1);
    assert!(json["trustworthy"].as_bool().is_some());
    assert!(json["server_metrics"].is_null());
    assert!(json["warnings"].is_array());
    let load = &json["loads"][0];
    assert_eq!(load["kind"], "acct-start");
    assert_eq!(load["mode"], "fixed-concurrency");
    assert_eq!(load["concurrency"], 4);
    assert!(load["rate"].is_null());
    assert_eq!(load["threads"], 2);
    assert_eq!(load["sockets"], 2);
    assert!(load["send_lag_us"].is_null());
    assert!(load["worker_busy_max"].is_f64());
    let first = &load["steps"][0];
    assert_eq!(first["step"], "acct-start");
    assert!(first["sent"].as_u64().unwrap() > 0);
    assert_eq!(first["responses"]["accounting"], first["sent"]);
    for key in ["accept", "reject", "challenge"] {
        assert_eq!(first["responses"][key], 0);
    }
    for key in ["timeouts", "invalid", "late", "socket_errors", "retransmits"] {
        assert_eq!(first[key], 0, "{key}");
    }
    assert!(first["throughput_per_s"].as_f64().unwrap() > 0.0);
    for key in ["p50", "p90", "p99", "p999", "max"] {
        assert!(first["latency_us"][key].as_f64().unwrap() > 0.0, "{key}");
    }
}

#[test]
fn test_text_report() {
    let responder = Responder::answering();
    let port = responder.address.port().to_string();
    let args: Vec<String> = ["run", "--server", "127.0.0.1", "--secret", SECRET, "--auth-port", &port, "--load", "pap,concurrency=1", "--requests", "3"].iter().map(|s| s.to_string()).collect();
    let output = bench(&args);
    assert_eq!(output.code, 0);
    assert!(output.stdout.contains("accept 3"), "{}", output.stdout);
    assert!(output.stdout.contains("p99.9"), "{}", output.stdout);
}

#[test]
fn test_exit_codes() {
    let accepted = Responder::answering();
    assert_eq!(run_against(accepted.address, &["--load", "pap,concurrency=4", "--requests", "20"]).code, 0);

    let some_rejected = Responder::start(|socket, from, request, count| {
        let response = if count % 2 == 0 { answer(request) } else { reject(request) };
        socket.send_to(&response, from).unwrap();
    });
    assert_eq!(run_against(some_rejected.address, &["--load", "pap,concurrency=1", "--requests", "10"]).code, 1);

    let challenged = Responder::start(|socket, from, request, _| {
        socket.send_to(&respond(request, ACCESS_CHALLENGE, &[]), from).unwrap();
    });
    assert_eq!(run_against(challenged.address, &["--load", "pap,concurrency=1", "--requests", "1"]).code, 1);

    let rejected_and_silent = Responder::start(|socket, from, request, count| {
        if count % 2 == 0 {
            socket.send_to(&reject(request), from).unwrap();
        }
    });
    let output = run_against(rejected_and_silent.address, &["--load", "pap,concurrency=1", "--requests", "4", "--timeout", "0.1"]);
    assert_eq!(output.code, 2);
    let json = output.json();
    assert_eq!(step(&json, 0, 0)["timeouts"], 2);
    assert_eq!(step(&json, 0, 0)["responses"]["reject"], 2);
}

#[test]
fn test_warmup_excluded() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "pap,rate=100", "--warmup", "0.5", "--duration", "0.5"]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    let sent = step(&json, 0, 0)["sent"].as_u64().unwrap();
    assert!((45..=55).contains(&sent), "{sent}");
    let total = responder.requests().len();
    assert!((95..=100).contains(&total), "{total}");
}

#[test]
fn test_throughput_is_responses_over_the_window() {
    let responder = Responder::answering();
    let json = run_against(responder.address, &["--load", "pap,rate=100", "--warmup", "0", "--duration", "1"]).json();
    let throughput = step(&json, 0, 0)["throughput_per_s"].as_f64().unwrap();
    assert!((95.0..=105.0).contains(&throughput), "{throughput}");
    assert_eq!(json["duration_s"], 1.0);
}

#[test]
fn test_concurrency_is_held() {
    let most = Arc::new(AtomicUsize::new(0));
    let seen = most.clone();
    // Holds requests until no more come, then answers them all.
    let held = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
    let address = socket.local_addr().unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let thread = std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        let mut from = None;
        while !stop.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buffer) {
                Ok((length, sender)) => {
                    from = Some(sender);
                    held.lock().unwrap().push(buffer[..length].to_vec());
                }
                Err(_) => {
                    let mut held = held.lock().unwrap();
                    seen.fetch_max(held.len(), Ordering::Relaxed);
                    for request in held.drain(..) {
                        socket.send_to(&answer(&request), from.unwrap()).unwrap();
                    }
                }
            }
        }
    });
    let output = run_against(address, &["--load", "pap,concurrency=8,window=8", "--warmup", "0", "--duration", "0.3"]);
    finished.store(true, Ordering::Relaxed);
    thread.join().unwrap();
    assert_eq!(output.code, 0, "{}", output.stderr);
    assert_eq!(most.load(Ordering::Relaxed), 8);
}

#[test]
fn test_forged_response() {
    let responder = Responder::start(|socket, from, request, _| {
        let mut forged = answer(request);
        forged[4] ^= 1;
        socket.send_to(&forged, from).unwrap();
    });
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--timeout", "0.1"]);
    assert_eq!(output.code, 2);
    let json = output.json();
    let first = step(&json, 0, 0);
    assert_eq!(first["invalid"], 1);
    assert_eq!(first["timeouts"], 1);
    assert_eq!(first["responses"]["accept"], 0);
}

#[test]
fn test_late_answer_on_reused_identifier() {
    let first_request = Mutex::new(None::<Vec<u8>>);
    let responder = Responder::start(move |socket, from, request, count| {
        let mut first = first_request.lock().unwrap();
        if count == 0 {
            *first = Some(request.to_vec());
            return;
        }
        // The Identifier has come round: the answer to the first request arrives now.
        if let Some(old) = first.as_ref().filter(|old| old[1] == request[1]) {
            socket.send_to(&answer(old), from).unwrap();
        }
        socket.send_to(&answer(request), from).unwrap();
    });
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "300", "--timeout", "0.1"]);
    let json = output.json();
    let first = step(&json, 0, 0);
    assert_eq!(first["timeouts"], 1);
    assert_eq!(first["invalid"], 1, "{first}");
    assert_eq!(first["late"], 0, "{first}");
    assert_eq!(first["responses"]["accept"], 299);
}

#[test]
fn test_late_answer_with_no_request() {
    let responder = Responder::start(|socket, from, request, count| {
        socket.send_to(&answer(request), from).unwrap();
        if count == 0 {
            // A second copy, when nothing is waiting on this Identifier any more.
            socket.send_to(&answer(request), from).unwrap();
        }
    });
    let output = run_against(responder.address, &["--load", "pap,rate=20", "--warmup", "0", "--duration", "0.2"]);
    assert_eq!(output.code, 0, "late answers do not change the exit code");
    assert_eq!(step(&output.json(), 0, 0)["late"], 1);
}

#[test]
fn test_retransmission() {
    let responder = Responder::start(|socket, from, request, count| {
        if count > 0 {
            socket.send_to(&answer(request), from).unwrap();
        }
    });
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--timeout", "0.2", "--retries", "1"]);
    assert_eq!(output.code, 0);
    let json = output.json();
    let first = step(&json, 0, 0);
    assert_eq!(first["responses"]["accept"], 1);
    assert_eq!(first["retransmits"], 1);
    assert_eq!(first["timeouts"], 0);
    assert!(first["latency_us"]["p50"].as_f64().unwrap() >= 190_000.0, "measured from the first send: {first}");
    let received = responder.received.lock().unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0], received[1], "a retransmission repeats the bytes");
}

#[test]
fn test_retries_run_out() {
    let responder = Responder::start(|_, _, _, _| {});
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--timeout", "0.05", "--retries", "2"]);
    assert_eq!(output.code, 2);
    let json = output.json();
    assert_eq!(step(&json, 0, 0)["timeouts"], 1);
    assert_eq!(step(&json, 0, 0)["retransmits"], 2);
    assert_eq!(responder.requests().len(), 3);
}

#[test]
fn test_closed_port_counts_socket_errors() {
    let port = {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap()
    };
    let output = run_against(port, &["--load", "pap,concurrency=1", "--requests", "2", "--timeout", "0.1"]);
    assert_eq!(output.code, 2);
    assert!(step(&output.json(), 0, 0)["socket_errors"].as_u64().unwrap() >= 1, "{}", output.stdout);
}

#[test]
fn test_stall_delays_later_requests() {
    let responder = Responder::start(|socket, from, request, count| {
        if count == 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        socket.send_to(&answer(request), from).unwrap();
    });
    let output = run_against(responder.address, &["--load", "pap,rate=100,concurrency=1", "--warmup", "0", "--duration", "1"]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    let p90 = step(&json, 0, 0)["latency_us"]["p90"].as_f64().unwrap();
    assert!(p90 >= 300_000.0, "{p90}");
    assert_eq!(json["loads"][0]["mode"], "fixed-rate");
    assert!(json["loads"][0]["send_lag_us"]["p99"].as_f64().unwrap() >= 300_000.0);
    assert!(json["loads"][0]["worker_busy_max"].is_null());
}

/// One request at a time, so that the rate is out of reach without the responder falling
/// behind: unlimited, a CI runner's responder dropped 84 of 9953 requests, and the timeouts
/// rightly made the exit code 2.
#[test]
fn test_rate_not_met() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "pap,rate=100000000,concurrency=1", "--warmup", "0", "--duration", "0.2"]);
    assert_eq!(output.code, 0, "warnings do not change the exit code: {}", output.stdout);
    let json = output.json();
    assert_eq!(json["trustworthy"], false);
    assert!(json["warnings"].as_array().unwrap().iter().any(|warning| warning["code"] == "rate_not_met"), "{json}");
}

/// Whether a machine keeps up depends on how busy it is, so this checks that the report
/// agrees with what it measured. The limits themselves are tested in `report`.
#[test]
fn test_rate_warning_follows_the_measurement() {
    let responder = Responder::answering();
    let json = run_against(responder.address, &["--load", "pap,rate=200", "--warmup", "0", "--duration", "0.5"]).json();
    let sent = step(&json, 0, 0)["sent"].as_u64().unwrap();
    let lag = json["loads"][0]["send_lag_us"]["p99"].as_f64().unwrap();
    assert!(sent > 0, "{json}");
    let missed = sent < 99 || lag > 1000.0;
    assert_eq!(json["trustworthy"], !missed, "{json}");
    let warnings = json["warnings"].as_array().unwrap();
    assert_eq!(warnings.iter().any(|warning| warning["code"] == "rate_not_met"), missed, "{json}");
    assert_eq!(warnings.len(), usize::from(missed), "{json}");
}

fn accepted_with_class(request: &[u8]) -> Vec<u8> {
    if request[0] == 1 {
        respond(request, ACCESS_ACCEPT, &[(CLASS, b"gold"), (FRAMED_IP_ADDRESS, &[10, 0, 0, 9])])
    } else {
        answer(request)
    }
}

#[test]
fn test_class_reaches_accounting() {
    let responder = Responder::start(|socket, from, request, _| {
        socket.send_to(&accepted_with_class(request), from).unwrap();
    });
    let output = run_against(responder.address, &["--load", "session,concurrency=1,alive=2", "--requests", "2"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    let json = output.json();
    let steps: Vec<_> = json["loads"][0]["steps"].as_array().unwrap().iter().map(|step| step["step"].as_str().unwrap().to_string()).collect();
    assert_eq!(steps, ["pap", "acct-start", "acct-alive", "acct-stop"]);
    assert_eq!(step(&json, 0, 0)["responses"]["accept"], 2);
    assert_eq!(step(&json, 0, 1)["responses"]["accounting"], 2);
    assert_eq!(step(&json, 0, 2)["responses"]["accounting"], 4);
    assert_eq!(step(&json, 0, 3)["responses"]["accounting"], 2);

    let requests = responder.requests();
    assert_eq!(requests.len(), 2 * 5);
    let statuses: Vec<Option<u8>> = requests[..5].iter().map(|packet| packet.attribute(ACCT_STATUS_TYPE).map(|status| status[3])).collect();
    assert_eq!(statuses, [None, Some(1), Some(3), Some(3), Some(2)]);
    for flow in requests.chunks(5) {
        let ids: HashSet<_> = flow[1..].iter().map(|packet| packet.attribute(ACCT_SESSION_ID).unwrap().to_vec()).collect();
        assert_eq!(ids.len(), 1, "one session");
        for packet in &flow[1..] {
            assert_eq!(packet.attribute(CLASS), Some(&b"gold"[..]));
            assert_eq!(packet.attribute(FRAMED_IP_ADDRESS), Some(&[10, 0, 0, 9][..]));
            assert!(packet.verify_accounting_request(SECRET.as_bytes()));
        }
        assert!(flow[0].attribute(CLASS).is_none());
    }
    assert_ne!(requests[1].attribute(ACCT_SESSION_ID), requests[6].attribute(ACCT_SESSION_ID), "each flow its own session");
}

#[test]
fn test_rejected_flow() {
    let responder = Responder::start(|socket, from, request, _| {
        socket.send_to(&reject(request), from).unwrap();
    });
    let output = run_against(responder.address, &["--load", "session,concurrency=1", "--requests", "3"]);
    assert_eq!(output.code, 1);
    let json = output.json();
    assert_eq!(step(&json, 0, 0)["responses"]["reject"], 3);
    assert_eq!(step(&json, 0, 1)["sent"], 0);
    assert!(responder.requests().iter().all(|packet| packet.code == 1));
}

#[test]
fn test_closed_port_shows_as_timeouts_in_a_session_load() {
    let port = {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap()
    };
    let output = run_against(port, &["--load", "session,concurrency=1", "--requests", "1", "--timeout", "0.1"]);
    assert_eq!(output.code, 2);
    let json = output.json();
    assert_eq!((step(&json, 0, 0)["timeouts"].as_u64(), step(&json, 0, 0)["socket_errors"].as_u64()), (Some(1), Some(0)), "{}", output.stdout);
}

#[test]
fn test_session_ends_on_timeout() {
    let responder = Responder::start(|socket, from, request, _| {
        if request[0] == 1 {
            socket.send_to(&answer(request), from).unwrap();
        }
    });
    let output = run_against(responder.address, &["--load", "session,concurrency=1", "--requests", "1", "--timeout", "0.05"]);
    assert_eq!(output.code, 2);
    let json = output.json();
    assert_eq!(step(&json, 0, 1)["timeouts"], 1);
    assert_eq!(step(&json, 0, 2)["sent"], 0);
}

#[test]
fn test_two_loads_run_together() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "acct-start,concurrency=4", "--load", "pap,rate=100", "--warmup", "0", "--duration", "0.5"]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    assert_eq!(json["loads"].as_array().unwrap().len(), 2);
    assert!(step(&json, 0, 0)["responses"]["accounting"].as_u64().unwrap() > 50);
    let accepted = step(&json, 1, 0)["responses"]["accept"].as_u64().unwrap();
    assert!((45..=55).contains(&accepted), "{accepted}");
    assert_eq!(step(&json, 1, 0)["responses"]["accounting"], 0);
}

#[test]
fn test_user_rotation() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "7", "--user", "u", "--users", "3"]);
    assert_eq!(output.code, 0);
    let names: Vec<_> = responder.requests().iter().map(|packet| String::from_utf8(packet.attribute(USER_NAME).unwrap().to_vec()).unwrap()).collect();
    assert_eq!(names, ["u0", "u1", "u2", "u0", "u1", "u2", "u0"]);

    let single = Responder::answering();
    run_against(single.address, &["--load", "pap,concurrency=1", "--requests", "1", "--user", "u"]);
    assert_eq!(single.requests()[0].attribute(USER_NAME), Some(&b"u"[..]));
}

#[test]
fn test_requests_differ_from_earlier_ones() {
    let responder = Responder::answering();
    run_against(responder.address, &["--load", "acct-start,concurrency=1", "--requests", "600"]);
    let requests = responder.requests();
    assert_eq!(requests.len(), 600);
    let sessions: HashSet<_> = requests.iter().map(|packet| packet.attribute(ACCT_SESSION_ID).unwrap().to_vec()).collect();
    assert_eq!(sessions.len(), 600);
    let identifiers: HashSet<_> = requests.iter().map(|packet| packet.identifier).collect();
    assert_eq!(identifiers.len(), 256, "the Identifier goes all the way round");

    let auth = Responder::answering();
    run_against(auth.address, &["--load", "pap,concurrency=1", "--requests", "300"]);
    let authenticators: HashSet<_> = auth.requests().iter().map(|packet| packet.authenticator).collect();
    assert_eq!(authenticators.len(), 300);
}

/// Serves each of `bodies` to one request, the last one to any more.
fn metrics_endpoint(bodies: Vec<&'static str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (served, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let mut head = [0u8; 1024];
            let _ = stream.read(&mut head);
            let body = bodies[served.min(bodies.len() - 1)];
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}").unwrap();
        }
    });
    format!("http://{address}/metrics")
}

#[test]
fn test_alive_session_time_grows() {
    let responder = Responder::answering();
    let output = run_against(responder.address, &["--load", "session,concurrency=1,alive=3", "--requests", "1"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    const ACCT_STATUS_TYPE: u8 = 40;
    const ACCT_SESSION_TIME: u8 = 46;
    let requests = responder.requests();
    let alive = requests.iter().filter(|packet| packet.attributes.iter().any(|(kind, value)| *kind == ACCT_STATUS_TYPE && value[..] == [0, 0, 0, 3]));
    let times: Vec<Vec<u8>> = alive.map(|packet| packet.attributes.iter().find(|(kind, _)| *kind == ACCT_SESSION_TIME).unwrap().1.clone()).collect();
    assert_eq!(times, vec![60u32.to_be_bytes().to_vec(), 120u32.to_be_bytes().to_vec(), 180u32.to_be_bytes().to_vec()]);
}

/// With too few file descriptors, setup fails at a different point depending on the limit.
/// Whichever point it is, the run has to end and say so.
#[test]
fn test_setup_failure_ends_the_run() {
    let responder = Responder::answering();
    let port = responder.address.port().to_string();
    let mut failed = 0;
    for limit in 8..40 {
        let mut child = std::process::Command::new("sh")
            .args(["-c", &format!("ulimit -n {limit}; exec \"$0\" \"$@\""), env!("CARGO_BIN_EXE_rradius-bench")])
            .args(["run", "--server", "127.0.0.1", "--secret", SECRET, "--auth-port", &port, "--acct-port", &port, "--load", "pap,concurrency=16,window=1", "--requests", "16", "--timeout", "1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("the run hung with a limit of {limit} file descriptors");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        failed += usize::from(status.code() == Some(3));
    }
    assert!(failed > 0, "no limit was low enough to test anything");
}

fn has_warning(json: &serde_json::Value, code: &str) -> bool {
    json["warnings"].as_array().unwrap().iter().any(|warning| warning["code"] == code)
}

/// A run of one request lasts well under a millisecond, and on a busy machine the worker
/// can really have no idle time in it, which is the one warning these tests cannot rule out.
fn trusted_unless_saturated(json: &serde_json::Value) -> bool {
    json["trustworthy"] == true || has_warning(json, "tool_saturated")
}

#[test]
fn test_overload_shows_up() {
    let responder = Responder::answering();
    let url = metrics_endpoint(vec![
        "rradius_radius_packets_total{kind=\"auth\"} 100\nrradius_radius_discarded_total{reason=\"overload\"} 3\n",
        "rradius_radius_packets_total{kind=\"auth\"} 150\nrradius_radius_discarded_total{reason=\"overload\"} 10\n",
    ]);
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--metrics-url", &url]);
    assert_eq!(output.code, 0);
    let json = output.json();
    assert_eq!(json["server_metrics"]["packets"]["auth"], 50);
    assert_eq!(json["server_metrics"]["discarded"]["overload"], 7);
    assert!(trusted_unless_saturated(&json), "{json}");
    let warning = json["warnings"].as_array().unwrap().iter().find(|warning| warning["code"] == "server_discarded_overload").expect("no overload warning");
    assert!(warning["message"].as_str().unwrap().contains("overload"), "{warning}");
}

#[test]
fn test_duplicate_path_warning() {
    let responder = Responder::answering();
    let url = metrics_endpoint(vec!["rradius_radius_duplicates_total 1\n", "rradius_radius_duplicates_total 9\n"]);
    let json = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--metrics-url", &url]).json();
    assert_eq!(json["server_metrics"]["duplicates"], 8);
    assert_eq!(json["trustworthy"], false);
    assert!(has_warning(&json, "duplicate_path"), "{json}");

    let url = metrics_endpoint(vec!["rradius_radius_duplicates_total 1\n", "rradius_radius_duplicates_total 9\n"]);
    let json = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--retries", "1", "--metrics-url", &url]).json();
    assert!(!has_warning(&json, "duplicate_path"), "with retransmissions, duplicates are expected: {json}");
    assert!(trusted_unless_saturated(&json), "{json}");
}

#[test]
fn test_metrics_unavailable() {
    let responder = Responder::answering();
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let url = format!("http://{closed}/metrics");
    let output = run_against(responder.address, &["--load", "pap,concurrency=1", "--requests", "1", "--metrics-url", &url]);
    assert_eq!(output.code, 0);
    let json = output.json();
    assert!(has_warning(&json, "metrics_unavailable"), "{json}");
    assert!(trusted_unless_saturated(&json), "{json}");
    assert!(json["server_metrics"].is_null());
    assert_eq!(step(&json, 0, 0)["responses"]["accept"], 1);
}

#[test]
fn test_calibration_output() {
    let output = bench(&["calibrate".to_string(), "--duration".to_string(), "0.3".to_string(), "--json".to_string()]);
    assert_eq!(output.code, 0, "{}", output.stderr);
    let json = output.json();
    assert_eq!(json["schema"], 1);
    let results = json["results"].as_array().unwrap();
    let shape: Vec<(u64, u64)> = results.iter().map(|result| (result["threads"].as_u64().unwrap(), result["window"].as_u64().unwrap())).collect();
    assert_eq!(shape, [(1, 1), (1, 64), (2, 64), (4, 64)]);
    for result in results {
        assert!(result["throughput_per_s"].as_f64().unwrap() > 0.0, "{result}");
        assert!(result["latency_us"]["p50"].as_f64().unwrap() > 0.0, "{result}");
    }
}

/// What each connection to a scripted TACACS+ server sent.
type Connections = Arc<Mutex<Vec<Arc<Mutex<Vec<u8>>>>>>;

/// Packets in what one TACACS+ connection sent, by the header's length.
fn tacacs_packets(mut bytes: &[u8]) -> usize {
    let mut count = 0;
    while let Some(header) = bytes.get(..12) {
        let length = 12 + u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
        let Some(rest) = bytes.get(length..) else { break };
        count += 1;
        bytes = rest;
    }
    count
}

/// A TACACS+ server that takes connections and never answers: requests time out, each
/// connection carries one, and a new one is opened after each timeout.
#[test]
fn test_tacacs_no_answer_times_out() -> Result<(), String> {
    let responder = Responder::answering();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port().to_string();
    let connections: Connections = Arc::new(Mutex::new(Vec::new()));
    let seen = connections.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let received = Arc::new(Mutex::new(Vec::new()));
            if let Ok(mut all) = seen.lock() {
                all.push(received.clone());
            }
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        return;
                    }
                    if let Ok(mut received) = received.lock() {
                        received.extend_from_slice(&buffer[..read]);
                    }
                }
            });
        }
    });
    let output = run_against(responder.address, &["--load", "tacacs-pap,concurrency=1", "--requests", "3", "--timeout", "0.3", "--retries", "2", "--tacacs-port", &port]);
    let json = output.json();
    let counts = step(&json, 0, 0);
    if output.code != 2 || counts["timeouts"] != 3 || counts["retransmits"] != 0 {
        return Err(format!("exit {}: {} / {}", output.code, output.stdout, output.stderr));
    }
    if json["loads"][0]["reconnects"].as_u64().unwrap_or(0) < 2 {
        return Err(format!("no new connections after timeouts: {}", output.stdout));
    }
    let connections = connections.lock().map_err(|e| e.to_string())?;
    let per_connection = connections.iter().map(|received| received.lock().map(|bytes| tacacs_packets(&bytes)).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
    if per_connection.iter().any(|packets| *packets > 1) || per_connection.iter().sum::<usize>() != 3 {
        return Err(format!("requests per connection: {per_connection:?}"));
    }
    Ok(())
}

/// Under `keep`, a connection that cannot be opened stops the run before anything is sent.
#[test]
fn test_tacacs_keep_unreachable_exits_3() -> Result<(), String> {
    let responder = Responder::answering();
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.local_addr().map_err(|e| e.to_string())?.port().to_string()
    };
    let output = run_against(responder.address, &["--load", "tacacs-pap,concurrency=1", "--requests", "1", "--timeout", "1", "--tacacs-port", &port]);
    if output.code != 3 || !output.stderr.contains("TACACS+ connection 1") {
        return Err(format!("exit {}: {}", output.code, output.stderr));
    }
    Ok(())
}

/// A server at its connection limit takes each connection and closes it: requests count as
/// socket errors, and connections are opened again at a bounded pace, not in a tight loop.
#[test]
fn test_tacacs_closed_unanswered_backs_off() -> Result<(), String> {
    let responder = Responder::answering();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let output = run_against(responder.address, &["--load", "tacacs-pap,concurrency=1", "--warmup", "0", "--duration", "1", "--timeout", "1", "--tacacs-port", &port]);
    let json = output.json();
    let reconnects = json["loads"][0]["reconnects"].as_u64().unwrap_or(0);
    let socket_errors = step(&json, 0, 0)["socket_errors"].as_u64().unwrap_or(0);
    // One try per 100 ms: about 10 in a second, against thousands without the pause.
    if output.code != 2 || socket_errors == 0 || reconnects == 0 || reconnects > 30 {
        return Err(format!("exit {}, {reconnects} reconnects, {socket_errors} socket errors: {}", output.code, output.stderr));
    }
    Ok(())
}

/// A TACACS+ server that answers an ASCII login, waiting `delay` before each reply.
fn slow_ascii_server(delay: Duration) -> Result<String, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut header = [0u8; 12];
                while stream.read_exact(&mut header).is_ok() {
                    let length = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
                    let mut body = vec![0u8; length];
                    if stream.read_exact(&mut body).is_err() {
                        return;
                    }
                    let seq = header[2].wrapping_add(1);
                    let mut reply: Vec<u8> = match seq {
                        2 => vec![0x04, 0, 0, 0, 0, 0],
                        4 => vec![0x05, 1, 0, 0, 0, 0],
                        _ => vec![0x01, 0, 0, 0, 0, 0],
                    };
                    let session = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                    rradius_bench::tacacs::crypt(&mut reply, SECRET.as_bytes(), session, header[0], seq);
                    let mut packet = vec![header[0], header[1], seq, 0];
                    packet.extend_from_slice(&header[4..8]);
                    packet.extend_from_slice(&(reply.len() as u32).to_be_bytes());
                    packet.extend_from_slice(&reply);
                    std::thread::sleep(delay);
                    if stream.write_all(&packet).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(port)
}

/// `--timeout` covers a whole ASCII login, not each of its round trips.
#[test]
fn test_tacacs_ascii_timeout_covers_the_login() -> Result<(), String> {
    let responder = Responder::answering();
    let port = slow_ascii_server(Duration::from_millis(400))?;
    let in_time = run_against(responder.address, &["--load", "tacacs-ascii,concurrency=1", "--requests", "1", "--timeout", "3", "--tacacs-port", &port]);
    if in_time.code != 0 || step(&in_time.json(), 0, 0)["responses"]["accept"] != 1 {
        return Err(format!("within the timeout: exit {}: {} / {}", in_time.code, in_time.stdout, in_time.stderr));
    }
    let too_slow = run_against(responder.address, &["--load", "tacacs-ascii,concurrency=1", "--requests", "1", "--timeout", "1", "--tacacs-port", &port]);
    let json = too_slow.json();
    if too_slow.code != 2 || step(&json, 0, 0)["timeouts"] != 1 || step(&json, 0, 0)["responses"]["accept"] != 0 {
        return Err(format!("three replies of 0.4 s against --timeout 1: exit {}: {} / {}", too_slow.code, too_slow.stdout, too_slow.stderr));
    }
    Ok(())
}
