//! `rradiusd accounting find`: who held a public IPv4 address and port at a time, from the lines of
//! the accounting log, when port blocks are reported with `IP-Port-Range` (RFC 8045).
//!
//! The lines are read, the ones that report a block or end a session are kept, and the holds are
//! worked out in the order of the event times. Nothing is stored and no cluster is asked.

use crate::accounting_log::parse_timestamp;
use serde_json::Value;
use std::net::Ipv4Addr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The attributes of a line that say whom a hold belongs to, as the output shows them.
const IDENTIFYING: &[&str] = &["User-Name", "Acct-Session-Id", "Framed-IP-Address", "NAS-Identifier", "NAS-IP-Address"];

/// Which time gave the allocation time of a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSource {
    EventTimestamp,
    ReceivedMinusDelay,
    Received,
}

impl TimeSource {
    pub fn name(self) -> &'static str {
        match self {
            TimeSource::EventTimestamp => "Event-Timestamp",
            TimeSource::ReceivedMinusDelay => "received-minus-delay",
            TimeSource::Received => "received",
        }
    }
}

/// How a hold ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndedBy {
    Release,
    Stop,
}

impl EndedBy {
    pub fn name(self) -> &'static str {
        match self {
            EndedBy::Release => "release",
            EndedBy::Stop => "stop",
        }
    }
}

/// A block of ports of a public address that a request reported as given to someone.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub ext_ip: Ipv4Addr,
    pub port_start: u16,
    pub port_end: u16,
    pub allocated: SystemTime,
    pub time_source: TimeSource,
    /// When it ended, and how; both are `None` while nothing in the lines ended it.
    pub ended: Option<(SystemTime, EndedBy)>,
    /// What the range was matched by, as text.
    pub identity: String,
    /// The `client` and `Acct-Session-Id` of the line that allocated it, which a `Stop` is
    /// matched with.
    client: String,
    session_id: Option<String>,
    /// The identifying attributes the allocating line has, and its `client`.
    pub attributes: Vec<(String, String)>,
    key: Key,
}

/// What a range is matched by when it is released.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    LocalId(String),
    Session(String, String),
    User(String, String, String),
}

impl Key {
    fn text(&self) -> String {
        match self {
            Key::LocalId(id) => format!("Local-Id={id}"),
            Key::Session(client, session) => format!("Acct-Session-Id={session} client={client}"),
            Key::User(client, user, address) => format!("User-Name={user} Framed-IP-Address={address} client={client}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Allocation,
    Deallocation,
}

#[derive(Debug, Clone, PartialEq)]
struct Range {
    action: Action,
    ext_ip: Option<Ipv4Addr>,
    ports: Option<(u16, u16)>,
    local_id: Option<String>,
}

/// One line that reports ranges or ends a session.
#[derive(Debug)]
struct Event {
    time: SystemTime,
    source: TimeSource,
    client: String,
    session_id: Option<String>,
    user_name: Option<String>,
    framed_ip: Option<String>,
    stop: bool,
    ranges: Vec<Range>,
    attributes: Vec<(String, String)>,
}

/// The lines that matter.
#[derive(Debug, Default)]
pub struct Events {
    events: Vec<Event>,
}

/// What a query found.
#[derive(Debug, Default)]
pub struct Findings {
    pub holds: Vec<Hold>,
    /// Ranges that no hold could be made of, or that could not be matched.
    pub unmatched: u64,
}

/// The first text an attribute has, whether it came once or in a list.
fn text<'a>(attributes: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a str> {
    match attributes.get(name)? {
        Value::String(text) => Some(text),
        Value::Array(list) => list.first()?.as_str(),
        _ => None,
    }
}

fn seconds(text: &str) -> Option<u64> {
    text.parse().ok()
}

impl Events {
    /// Reads one line of the log. Only a line that is not a JSON object is an error.
    pub fn add_line(&mut self, line: &str) -> Result<(), String> {
        let Value::Object(line) = serde_json::from_str(line).map_err(|e| e.to_string())? else {
            return Err("not a JSON object".to_string());
        };
        let Some(Value::Object(attributes)) = line.get("attributes") else {
            return Ok(());
        };
        let stop = text(attributes, "Acct-Status-Type") == Some("Stop");
        let groups = match attributes.get("IP-Port-Range") {
            Some(Value::Array(groups)) => groups.as_slice(),
            _ => &[],
        };
        if !stop && groups.is_empty() {
            return Ok(());
        }
        let Some(received) = line.get("time").and_then(Value::as_str).and_then(|time| parse_timestamp(time).ok()) else {
            return Err("it has no valid \"time\"".to_string());
        };
        let (time, source) = if let Some(at) = text(attributes, "Event-Timestamp").and_then(seconds) {
            (UNIX_EPOCH + Duration::from_secs(at), TimeSource::EventTimestamp)
        } else if let Some(time) = text(attributes, "Acct-Delay-Time").and_then(seconds).and_then(|delay| received.checked_sub(Duration::from_secs(delay))) {
            (time, TimeSource::ReceivedMinusDelay)
        } else {
            (received, TimeSource::Received)
        };
        let client = line.get("client").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut shown = vec![("client".to_string(), client.clone())];
        shown.extend(IDENTIFYING.iter().filter_map(|name| text(attributes, name).map(|value| (name.to_string(), value.to_string()))));
        let mut ranges = Vec::new();
        for group in groups {
            let Value::Object(group) = group else { continue };
            let action = match text(group, "IP-Port-Range-Alloc") {
                Some("Allocation" | "1") => Action::Allocation,
                Some("Deallocation" | "2") => Action::Deallocation,
                _ => continue,
            };
            let port = |name: &str| text(group, name).and_then(|value| value.parse::<u16>().ok());
            let ports = match (port("IP-Port-Range-Range-Start"), port("IP-Port-Range-Range-End")) {
                (Some(start), Some(end)) if start <= end => Some((start, end)),
                _ => None,
            };
            ranges.push(Range {
                action,
                ext_ip: text(group, "IP-Port-Range-Ext-IPv4-Addr").and_then(|value| value.parse().ok()),
                ports,
                local_id: text(group, "IP-Port-Range-Local-Id").map(str::to_string),
            });
        }
        self.events.push(Event {
            time,
            source,
            session_id: text(attributes, "Acct-Session-Id").map(str::to_string),
            user_name: text(attributes, "User-Name").map(str::to_string),
            framed_ip: text(attributes, "Framed-IP-Address").map(str::to_string),
            client,
            stop,
            ranges,
            attributes: shown,
        });
        Ok(())
    }

    /// The holds of `ip` and `port` at `at`, from every line read.
    pub fn holds_at(&mut self, ip: Ipv4Addr, port: u16, at: SystemTime) -> Findings {
        // The sort keeps the order of reading for equal times.
        self.events.sort_by_key(|event| event.time);
        let mut holds: Vec<Hold> = Vec::new();
        let mut unmatched = 0;
        for event in &self.events {
            for range in &event.ranges {
                let key = match (&range.local_id, &event.session_id, &event.user_name, &event.framed_ip) {
                    (Some(id), ..) => Some(Key::LocalId(id.clone())),
                    (None, Some(session), ..) => Some(Key::Session(event.client.clone(), session.clone())),
                    (None, None, Some(user), Some(address)) => Some(Key::User(event.client.clone(), user.clone(), address.clone())),
                    _ => None,
                };
                let Some(key) = key else {
                    unmatched += 1;
                    continue;
                };
                match range.action {
                    Action::Allocation => {
                        let (Some(ext_ip), Some((port_start, port_end))) = (range.ext_ip, range.ports) else {
                            unmatched += 1;
                            continue;
                        };
                        holds.push(Hold {
                            ext_ip,
                            port_start,
                            port_end,
                            allocated: event.time,
                            time_source: event.source,
                            ended: None,
                            identity: key.text(),
                            client: event.client.clone(),
                            session_id: event.session_id.clone(),
                            attributes: event.attributes.clone(),
                            key,
                        });
                    }
                    Action::Deallocation => {
                        for hold in holds.iter_mut().filter(|hold| hold.ended.is_none() && hold.key == key) {
                            let same_address = range.ext_ip.is_none_or(|ext_ip| ext_ip == hold.ext_ip);
                            let overlaps = range.ports.is_none_or(|(start, end)| start <= hold.port_end && hold.port_start <= end);
                            if same_address && overlaps {
                                hold.ended = Some((event.time, EndedBy::Release));
                            }
                        }
                    }
                }
            }
            if event.stop {
                if let Some(session) = &event.session_id {
                    for hold in holds.iter_mut().filter(|hold| hold.ended.is_none() && hold.client == event.client && hold.session_id.as_ref() == Some(session)) {
                        hold.ended = Some((event.time, EndedBy::Stop));
                    }
                }
            }
        }
        holds.retain(|hold| hold.ext_ip == ip && hold.port_start <= port && port <= hold.port_end && hold.allocated <= at && hold.ended.is_none_or(|(ended, _)| ended > at));
        Findings { holds, unmatched }
    }
}

pub const USAGE: &str = "usage: rradiusd accounting find --ip <address> --port <port> --at <time> [--file <path>]... [--json]\n  --file - or no --file reads standard input; --at is RFC 3339, such as 2026-11-25T10:00:00Z";

/// What the arguments ask for.
#[derive(Debug, PartialEq)]
struct Command {
    ip: Ipv4Addr,
    port: u16,
    at: SystemTime,
    files: Vec<String>,
    json: bool,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let usage = crate::cli::named(USAGE);
    let mut args = args.iter();
    if args.next().map(String::as_str) != Some("find") {
        return Err(usage);
    }
    let (mut ip, mut port, mut at, mut files, mut json) = (None, None, None, Vec::new(), false);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().cloned().ok_or_else(|| format!("{name} needs a value\n{usage}"));
        match arg.as_str() {
            "--ip" if ip.is_none() => {
                let text = value("--ip")?;
                ip = Some(text.parse::<Ipv4Addr>().map_err(|_| format!("--ip: {text:?} is not an IPv4 address"))?);
            }
            "--port" if port.is_none() => {
                let text = value("--port")?;
                port = Some(text.parse::<u16>().map_err(|_| format!("--port: {text:?} is not an integer from 0 to 65535"))?);
            }
            "--at" if at.is_none() => at = Some(parse_timestamp(&value("--at")?).map_err(|e| format!("--at: {e}"))?),
            "--file" => files.push(value("--file")?),
            "--json" if !json => json = true,
            _ => return Err(format!("unexpected argument '{arg}'\n{usage}")),
        }
    }
    let missing = |name: &str| format!("{name} is required\n{usage}");
    Ok(Command { ip: ip.ok_or_else(|| missing("--ip"))?, port: port.ok_or_else(|| missing("--port"))?, at: at.ok_or_else(|| missing("--at"))?, files, json })
}

/// Reads the lines of `reader` into `events`; an error says where.
fn read_lines(name: &str, reader: impl std::io::BufRead, events: &mut Events) -> Result<(), String> {
    for (number, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("{name}: line {}: {e}", number + 1))?;
        events.add_line(&line).map_err(|e| format!("{name}: line {}: {e}", number + 1))?;
    }
    Ok(())
}

/// Reads the file `path` into `events`. A name that ends in `.gz` (in any letter case) is
/// decompressed, a file of several gzip members one after the other included; any other file is
/// read as it is.
fn read_file(path: &str, events: &mut Events) -> Result<(), String> {
    let opened = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    if path.to_ascii_lowercase().ends_with(".gz") {
        read_lines(path, std::io::BufReader::new(flate2::read::MultiGzDecoder::new(opened)), events)
    } else {
        read_lines(path, std::io::BufReader::new(opened), events)
    }
}

/// A time as RFC 3339 in UTC, with the milliseconds only when there are some.
fn rfc3339(time: SystemTime) -> String {
    let full = crate::accounting_log::timestamp(time);
    match full.strip_suffix(".000Z") {
        Some(whole) => format!("{whole}Z"),
        None => full,
    }
}

fn holds_json(holds: &[Hold]) -> String {
    let holds: Vec<Value> = holds
        .iter()
        .map(|hold| {
            let attributes: serde_json::Map<String, Value> = hold.attributes.iter().map(|(name, value)| (name.clone(), Value::String(value.clone()))).collect();
            serde_json::json!({
                "ext_ip": hold.ext_ip.to_string(),
                "port_start": hold.port_start,
                "port_end": hold.port_end,
                "allocated": rfc3339(hold.allocated),
                "released": hold.ended.map(|(time, _)| rfc3339(time)),
                "ended_by": hold.ended.map(|(_, by)| by.name()),
                "time_source": hold.time_source.name(),
                "identity": hold.identity,
                "attributes": attributes,
            })
        })
        .collect();
    Value::Array(holds).to_string()
}

fn holds_text(holds: &[Hold]) -> String {
    let mut out = String::new();
    for hold in holds {
        let ended = hold.ended.map_or_else(|| "still held".to_string(), |(time, _)| rfc3339(time));
        let by = hold.ended.map_or("none", |(_, by)| by.name());
        out.push_str(&format!("{}:{}-{} allocated {} ended {ended} ended_by {by} time_source {} identity \"{}\"", hold.ext_ip, hold.port_start, hold.port_end, rfc3339(hold.allocated), hold.time_source.name(), hold.identity));
        for (name, value) in &hold.attributes {
            out.push_str(&format!(" {name}={value}"));
        }
        out.push('\n');
    }
    out
}

/// Runs `accounting` with its arguments, the word `accounting` not included. Prints the result and
/// returns the exit status: 0 when a hold is listed, 1 when none is, 2 for a mistake.
pub fn run(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut events = Events::default();
    let files: Vec<&str> = if command.files.is_empty() { vec!["-"] } else { command.files.iter().map(String::as_str).collect() };
    for file in files {
        let read = if file == "-" { read_lines("standard input", std::io::stdin().lock(), &mut events) } else { read_file(file, &mut events) };
        if let Err(e) = read {
            eprintln!("{e}");
            return 2;
        }
    }
    let found = events.holds_at(command.ip, command.port, command.at);
    if found.unmatched > 0 {
        eprintln!("{} port range(s) could not be matched: no Local-Id, Acct-Session-Id or User-Name with Framed-IP-Address, or an allocation without Ext-IPv4-Addr or Range-Start and Range-End", found.unmatched);
    }
    if command.json {
        println!("{}", holds_json(&found.holds));
    } else {
        print!("{}", holds_text(&found.holds));
    }
    i32::from(found.holds.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOC: &str = "IP-Port-Range-Alloc";

    /// A log line: received at `time` from `client`, with the flat attributes and the groups.
    fn line(time: &str, client: &str, attributes: &[(&str, &str)], groups: &[&[(&str, &str)]]) -> String {
        let mut attributes: serde_json::Map<String, Value> = attributes.iter().map(|(name, value)| (name.to_string(), Value::String(value.to_string()))).collect();
        if !groups.is_empty() {
            let groups = groups.iter().map(|group| Value::Object(group.iter().map(|(name, value)| (name.to_string(), Value::String(value.to_string()))).collect())).collect();
            attributes.insert("IP-Port-Range".to_string(), Value::Array(groups));
        }
        serde_json::json!({"time": time, "client": client, "source": "192.0.2.1", "transport": "udp", "handling": "script", "attributes": attributes}).to_string()
    }

    fn allocation<'a>(ext: &'a str, start: &'a str, end: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![(ALLOC, "Allocation"), ("IP-Port-Range-Ext-IPv4-Addr", ext), ("IP-Port-Range-Range-Start", start), ("IP-Port-Range-Range-End", end)]
    }

    fn at(text: &str) -> Result<SystemTime, String> {
        parse_timestamp(text)
    }

    fn events(lines: &[String]) -> Result<Events, String> {
        let mut events = Events::default();
        for (number, line) in lines.iter().enumerate() {
            events.add_line(line).map_err(|e| format!("line {}: {e}", number + 1))?;
        }
        Ok(events)
    }

    fn query(events: &mut Events, port: u16, when: &str) -> Result<Findings, String> {
        Ok(events.holds_at(Ipv4Addr::new(198, 51, 100, 7), port, at(when)?))
    }

    /// The lines of a session that is given 1024 to 2047 at 10:00 and gives them back at 11:00.
    fn session() -> Vec<String> {
        let session = [("Acct-Session-Id", "s-1"), ("User-Name", "alice"), ("Framed-IP-Address", "100.64.0.5")];
        let first = allocation("198.51.100.7", "1024", "2047");
        let release = [(ALLOC, "Deallocation"), ("IP-Port-Range-Ext-IPv4-Addr", "198.51.100.7"), ("IP-Port-Range-Range-Start", "1024"), ("IP-Port-Range-Range-End", "2047")];
        vec![
            line("2026-11-25T10:00:00.000Z", "bng1", &session, &[&first]),
            line("2026-11-25T11:00:00.000Z", "bng1", &session, &[&release]),
        ]
    }

    /// spec Port Block Holding, Scenario: Held between allocation and release, and its Example.
    #[test]
    fn test_held_between_allocation_and_release() -> Result<(), String> {
        let mut events = events(&session())?;
        let boundaries = [
            ("2026-11-25T09:59:59Z", 1500, false),
            ("2026-11-25T10:00:00Z", 1500, true),
            ("2026-11-25T10:59:59Z", 1500, true),
            ("2026-11-25T11:00:00Z", 1500, false),
            ("2026-11-25T10:30:00Z", 1024, true),
            ("2026-11-25T10:30:00Z", 2047, true),
            ("2026-11-25T10:30:00Z", 2048, false),
            ("2026-11-25T10:30:00Z", 1023, false),
        ];
        for (when, port, listed) in boundaries {
            assert_eq!(query(&mut events, port, when)?.holds.len() == 1, listed, "{when} {port}");
        }
        let found = query(&mut events, 1500, "2026-11-25T10:30:00Z")?;
        let hold = found.holds.first().ok_or("no hold")?;
        assert_eq!((hold.port_start, hold.port_end, hold.time_source, hold.ended.map(|(_, by)| by)), (1024, 2047, TimeSource::Received, Some(EndedBy::Release)));
        assert_eq!(hold.ended.map(|(time, _)| time), Some(at("2026-11-25T11:00:00Z")?));
        assert_eq!(hold.identity, "Acct-Session-Id=s-1 client=bng1");
        let shown: Vec<(&str, &str)> = hold.attributes.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        assert_eq!(shown, vec![("client", "bng1"), ("User-Name", "alice"), ("Acct-Session-Id", "s-1"), ("Framed-IP-Address", "100.64.0.5")]);
        Ok(())
    }

    /// spec Port Block Holding, Scenario: Release without a range.
    #[test]
    fn test_release_without_a_range_ends_all_of_the_session() -> Result<(), String> {
        let session = [("Acct-Session-Id", "s-1")];
        let first = allocation("198.51.100.7", "1024", "2047");
        let second = allocation("198.51.100.7", "4096", "4096");
        let release: Vec<(&str, &str)> = vec![(ALLOC, "Deallocation")];
        let mut events = events(&[
            line("2026-11-25T10:00:00Z", "bng1", &session, &[&first]),
            line("2026-11-25T10:01:00Z", "bng1", &session, &[&second]),
            line("2026-11-25T11:00:00Z", "bng1", &session, &[&release]),
        ])?;
        assert_eq!(query(&mut events, 1500, "2026-11-25T10:30:00Z")?.holds.len(), 1);
        assert_eq!(query(&mut events, 4096, "2026-11-25T10:30:00Z")?.holds.len(), 1);
        assert!(query(&mut events, 1500, "2026-11-25T11:30:00Z")?.holds.is_empty());
        assert!(query(&mut events, 4096, "2026-11-25T11:30:00Z")?.holds.is_empty());
        Ok(())
    }

    /// A release of an address and a range ends the holds that overlap it only, of its own
    /// session.
    #[test]
    fn test_release_with_a_range_and_other_sessions() -> Result<(), String> {
        let first = allocation("198.51.100.7", "1024", "2047");
        let same_ports_other_address = allocation("198.51.100.8", "1024", "2047");
        let release = [(ALLOC, "Deallocation"), ("IP-Port-Range-Ext-IPv4-Addr", "198.51.100.7"), ("IP-Port-Range-Range-Start", "2000"), ("IP-Port-Range-Range-End", "3000")];
        let mut events = events(&[
            line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&first]),
            line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-2")], &[&first]),
            line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&same_ports_other_address]),
            line("2026-11-25T11:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&release]),
        ])?;
        let found = query(&mut events, 1500, "2026-11-25T11:30:00Z")?;
        assert_eq!(found.holds.len(), 1, "{found:?}");
        assert_eq!(found.holds.first().map(|hold| hold.identity.as_str()), Some("Acct-Session-Id=s-2 client=bng1"));
        assert!(events.holds_at(Ipv4Addr::new(198, 51, 100, 8), 1500, at("2026-11-25T11:30:00Z")?).holds.len() == 1);
        Ok(())
    }

    /// spec Port Block Holding, Scenario: Several ranges in one request.
    #[test]
    fn test_several_ranges_in_one_request() -> Result<(), String> {
        let first = allocation("198.51.100.7", "1024", "2047");
        let second = allocation("198.51.100.7", "4096", "4096");
        let mut events = events(&[line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&first, &second])])?;
        assert_eq!(query(&mut events, 4096, "2026-11-25T10:30:00Z")?.holds.first().map(|hold| (hold.port_start, hold.port_end)), Some((4096, 4096)));
        assert_eq!(query(&mut events, 1500, "2026-11-25T10:30:00Z")?.holds.first().map(|hold| (hold.port_start, hold.port_end)), Some((1024, 2047)));
        assert!(query(&mut events, 3000, "2026-11-25T10:30:00Z")?.holds.is_empty());
        Ok(())
    }

    /// spec Port Block Holding, Scenarios: Ended by Stop, and Never released.
    #[test]
    fn test_stop_ends_a_session_and_nothing_ends_a_hold_that_is_never_released() -> Result<(), String> {
        let first = allocation("198.51.100.7", "1024", "2047");
        let mut events = events(&[
            line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&first]),
            line("2026-11-25T12:00:00Z", "bng1", &[("Acct-Session-Id", "s-1"), ("Acct-Status-Type", "Stop")], &[]),
            // Another client's Stop with the same session id ends nothing.
            line("2026-11-25T12:30:00Z", "bng2", &[("Acct-Session-Id", "s-1"), ("Acct-Status-Type", "Stop")], &[]),
        ])?;
        let before = query(&mut events, 1500, "2026-11-25T11:00:00Z")?;
        assert_eq!(before.holds.first().map(|hold| hold.ended), Some(Some((at("2026-11-25T12:00:00Z")?, EndedBy::Stop))));
        assert!(query(&mut events, 1500, "2026-11-25T12:00:00Z")?.holds.is_empty());
        let mut open = self::events(&[line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&first])])?;
        let held = query(&mut open, 1500, "2027-01-01T00:00:00Z")?;
        assert_eq!(held.holds.first().map(|hold| hold.ended), Some(None));
        Ok(())
    }

    /// spec Port Block Holding, Scenario: Identity by Local-Id, and the identity of a line with a
    /// user and an address only.
    #[test]
    fn test_identity_by_local_id_and_by_user() -> Result<(), String> {
        let mut with_id = allocation("198.51.100.7", "1024", "2047");
        with_id.push(("IP-Port-Range-Local-Id", "vrf-7"));
        let release = [(ALLOC, "Deallocation"), ("IP-Port-Range-Local-Id", "vrf-7")];
        let user = [("User-Name", "bob"), ("Framed-IP-Address", "100.64.0.9")];
        let other = allocation("198.51.100.7", "3000", "3999");
        let user_release: Vec<(&str, &str)> = vec![(ALLOC, "Deallocation")];
        let mut events = events(&[
            line("2026-11-25T10:00:00Z", "cgn1", &[("Acct-Session-Id", "s-1")], &[&with_id]),
            line("2026-11-25T11:00:00Z", "cgn1", &[("Acct-Session-Id", "other-session")], &[&release]),
            line("2026-11-25T10:00:00Z", "bng1", &user, &[&other]),
            line("2026-11-25T11:00:00Z", "bng1", &user, &[&user_release]),
        ])?;
        assert!(query(&mut events, 1500, "2026-11-25T11:30:00Z")?.holds.is_empty());
        assert_eq!(query(&mut events, 1500, "2026-11-25T10:30:00Z")?.holds.first().map(|hold| hold.identity.as_str()), Some("Local-Id=vrf-7"));
        assert_eq!(query(&mut events, 3500, "2026-11-25T10:30:00Z")?.holds.first().map(|hold| hold.identity.as_str()), Some("User-Name=bob Framed-IP-Address=100.64.0.9 client=bng1"));
        assert!(query(&mut events, 3500, "2026-11-25T11:30:00Z")?.holds.is_empty());
        Ok(())
    }

    /// spec Port Block Holding, Scenario: Ranges that cannot be matched.
    #[test]
    fn test_ranges_that_cannot_be_matched_are_counted() -> Result<(), String> {
        let without_address: Vec<(&str, &str)> = vec![(ALLOC, "Allocation"), ("IP-Port-Range-Range-Start", "1024"), ("IP-Port-Range-Range-End", "2047")];
        let without_end: Vec<(&str, &str)> = vec![(ALLOC, "Allocation"), ("IP-Port-Range-Ext-IPv4-Addr", "198.51.100.7"), ("IP-Port-Range-Range-Start", "1024")];
        let good = allocation("198.51.100.7", "5000", "5000");
        let mut events = events(&[
            line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Session-Id", "s-1")], &[&without_address, &without_end]),
            // No Local-Id, no session and no user and address.
            line("2026-11-25T10:00:00Z", "bng1", &[], &[&good]),
        ])?;
        let found = query(&mut events, 1500, "2026-11-25T10:30:00Z")?;
        assert!(found.holds.is_empty());
        assert_eq!(found.unmatched, 3);
        Ok(())
    }

    /// spec Event Time of a Line, its three Scenarios.
    #[test]
    fn test_event_time_of_a_line() -> Result<(), String> {
        let first = allocation("198.51.100.7", "1024", "2047");
        let sources = |attributes: &[(&str, &str)]| -> Result<(SystemTime, TimeSource), String> {
            let mut all = vec![("Acct-Session-Id", "s-1")];
            all.extend_from_slice(attributes);
            let mut events = events(&[line("2026-11-25T10:05:00Z", "bng1", &all, &[&first])])?;
            let found = events.holds_at(Ipv4Addr::new(198, 51, 100, 7), 1500, at("2027-01-01T00:00:00Z")?);
            found.holds.first().map(|hold| (hold.allocated, hold.time_source)).ok_or_else(|| "no hold".to_string())
        };
        // 2026-11-25T10:00:00Z
        let event = at("2026-11-25T10:00:00Z")?;
        let stamp = event.duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs().to_string();
        assert_eq!(sources(&[("Event-Timestamp", stamp.as_str()), ("Acct-Delay-Time", "120")])?, (event, TimeSource::EventTimestamp));
        assert_eq!(sources(&[("Acct-Delay-Time", "120")])?, (at("2026-11-25T10:03:00Z")?, TimeSource::ReceivedMinusDelay));
        assert_eq!(sources(&[])?, (at("2026-11-25T10:05:00Z")?, TimeSource::Received));
        Ok(())
    }

    /// The order the events happened in decides, not the order the lines were read in.
    #[test]
    fn test_events_are_taken_in_the_order_of_their_event_times() -> Result<(), String> {
        let mut lines = session();
        lines.reverse();
        let mut events = events(&lines)?;
        assert_eq!(query(&mut events, 1500, "2026-11-25T10:30:00Z")?.holds.len(), 1);
        assert!(query(&mut events, 1500, "2026-11-25T11:30:00Z")?.holds.is_empty());
        Ok(())
    }

    /// Lines with nothing to report are not kept; a line that is not a JSON object is an error.
    #[test]
    fn test_lines_that_matter_and_lines_that_are_not_json() -> Result<(), String> {
        let mut events = Events::default();
        events.add_line(&line("2026-11-25T10:00:00Z", "bng1", &[("Acct-Status-Type", "Start"), ("Acct-Session-Id", "s-1")], &[]))?;
        assert!(events.events.is_empty());
        assert!(events.add_line("not json").is_err());
        assert!(events.add_line("[1,2]").is_err());
        assert!(events.add_line("").is_err());
        Ok(())
    }

    fn gzip(text: &str) -> Result<Vec<u8>, String> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        encoder.finish().map_err(|e| e.to_string())
    }

    /// Reads `bytes` as the file `name` and asks who held 1500 at 10:30 (Log File Opening).
    fn read_named(name: &str, bytes: &[u8]) -> Result<Result<usize, String>, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        let mut events = Events::default();
        let read = read_file(&path.to_string_lossy(), &mut events);
        Ok(read.map(|()| events.holds_at(Ipv4Addr::new(198, 51, 100, 7), 1500, at("2026-11-25T10:30:00Z").unwrap_or(UNIX_EPOCH)).holds.len()))
    }

    fn session_text() -> String {
        session().join("\n") + "\n"
    }

    /// A file is read by its name: `.gz` in any letter case is decompressed, the rest is not
    /// (Log File Opening, reading by name).
    #[test]
    fn test_files_are_read_by_name() -> Result<(), String> {
        let text = session_text();
        let packed = gzip(&text)?;
        assert_eq!(read_named("a.jsonl.gz", &packed)?, Ok(1));
        assert_eq!(read_named("a.JSONL.GZ", &packed)?, Ok(1));
        assert_eq!(read_named("a.jsonl", text.as_bytes())?, Ok(1));
        // A compressed file without the extension is plain text, and not JSON.
        assert!(read_named("a.gz.jsonl", &packed)?.is_err());
        Ok(())
    }

    /// Several gzip members one after the other are one file; the block is in the second.
    #[test]
    fn test_gzip_members_one_after_the_other() -> Result<(), String> {
        let lines = session();
        let mut both = gzip(&(lines[..1].join("\n") + "\n"))?;
        both.extend(gzip(&(lines[1..].join("\n") + "\n"))?);
        assert_eq!(read_named("a.jsonl.gz", &both)?, Ok(1));
        Ok(())
    }

    /// Data that is not gzip, or that ends early, is an error that names the file.
    #[test]
    fn test_damaged_gzip_is_an_error() -> Result<(), String> {
        let text = session_text();
        let Err(message) = read_named("a.jsonl.gz", text.as_bytes())? else {
            return Err("plain text in a .gz file was accepted".to_string());
        };
        assert!(message.contains("a.jsonl.gz"), "{message}");
        let packed = gzip(&text)?;
        let Err(message) = read_named("b.jsonl.gz", &packed[..packed.len() / 2])? else {
            return Err("a truncated gzip file was accepted".to_string());
        };
        assert!(message.contains("b.jsonl.gz"), "{message}");
        Ok(())
    }

    /// A missing file is an error that names it, with or without the extension.
    #[test]
    fn test_missing_files_are_named() {
        let mut events = Events::default();
        for name in ["/nonexistent/log.jsonl", "/nonexistent/log.jsonl.gz"] {
            assert!(read_file(name, &mut events).is_err_and(|e| e.contains(name)));
        }
    }
}
