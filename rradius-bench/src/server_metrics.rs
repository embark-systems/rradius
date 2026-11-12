//! Reads a server's `/metrics` to compare its counters before and after a run.
use serde::Serialize;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsUrl {
    authority: String,
    path: String,
}

impl MetricsUrl {
    pub fn parse(url: &str) -> Result<MetricsUrl, String> {
        let rest = url.strip_prefix("http://").ok_or_else(|| format!("--metrics-url {url}: must start with http://"))?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, "/metrics".to_string()),
        };
        if authority.is_empty() {
            return Err(format!("--metrics-url {url}: no host"));
        }
        Ok(MetricsUrl { authority: authority.to_string(), path })
    }

    /// Reads the counters. The server closes the connection after answering.
    pub fn read(&self) -> Result<Counters, String> {
        let address = self.authority.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
        let mut stream = TcpStream::connect_timeout(&address, WAIT).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        let request = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", self.path, self.authority);
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream.read_to_string(&mut response).map_err(|e| e.to_string())?;
        let (head, body) = response.split_once("\r\n\r\n").ok_or("not an HTTP response")?;
        let status = head.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("200") {
            return Err(format!("answered \"{status}\""));
        }
        Ok(Counters::parse(body))
    }
}

/// The server's counters that tell about a measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub packets_auth: u64,
    pub packets_acct: u64,
    pub duplicates: u64,
    pub discarded_overload: u64,
    pub discarded_unknown_client: u64,
    pub discarded_invalid: u64,
}

impl Counters {
    /// Reads the Prometheus text format; what is absent stays 0.
    pub fn parse(text: &str) -> Counters {
        let mut counters = Counters::default();
        for line in text.lines() {
            let Some((name, value)) = line.rsplit_once(' ') else { continue };
            let Ok(value) = value.parse::<u64>() else { continue };
            let slot = match name {
                "rradius_radius_packets_total{kind=\"auth\"}" => &mut counters.packets_auth,
                "rradius_radius_packets_total{kind=\"acct\"}" => &mut counters.packets_acct,
                "rradius_radius_duplicates_total" => &mut counters.duplicates,
                "rradius_radius_discarded_total{reason=\"overload\"}" => &mut counters.discarded_overload,
                "rradius_radius_discarded_total{reason=\"unknown_client\"}" => &mut counters.discarded_unknown_client,
                "rradius_radius_discarded_total{reason=\"invalid\"}" => &mut counters.discarded_invalid,
                _ => continue,
            };
            *slot = value;
        }
        counters
    }

    pub fn since(&self, earlier: &Counters) -> MetricsChange {
        MetricsChange {
            packets: PacketChange {
                auth: self.packets_auth.saturating_sub(earlier.packets_auth),
                acct: self.packets_acct.saturating_sub(earlier.packets_acct),
            },
            duplicates: self.duplicates.saturating_sub(earlier.duplicates),
            discarded: DiscardChange {
                overload: self.discarded_overload.saturating_sub(earlier.discarded_overload),
                unknown_client: self.discarded_unknown_client.saturating_sub(earlier.discarded_unknown_client),
                invalid: self.discarded_invalid.saturating_sub(earlier.discarded_invalid),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MetricsChange {
    pub packets: PacketChange,
    pub duplicates: u64,
    pub discarded: DiscardChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PacketChange {
    pub auth: u64,
    pub acct: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiscardChange {
    pub overload: u64,
    pub unknown_client: u64,
    pub invalid: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url() {
        let url = MetricsUrl::parse("http://127.0.0.1:9812/metrics").unwrap();
        assert_eq!((url.authority.as_str(), url.path.as_str()), ("127.0.0.1:9812", "/metrics"));
        assert_eq!(MetricsUrl::parse("http://h:1").unwrap().path, "/metrics");
        assert!(MetricsUrl::parse("https://h:1/metrics").is_err());
        assert!(MetricsUrl::parse("h:1").is_err());
        assert!(MetricsUrl::parse("http:///metrics").is_err());
    }

    #[test]
    fn test_counters() {
        let before = Counters::parse("# HELP x\nrradius_radius_packets_total{kind=\"auth\"} 5\nrradius_radius_duplicates_total 1\nother 3\n");
        let after = Counters::parse(
            "rradius_radius_packets_total{kind=\"auth\"} 9\nrradius_radius_packets_total{kind=\"acct\"} 2\nrradius_radius_duplicates_total 4\n\
             rradius_radius_discarded_total{reason=\"overload\"} 7\nrradius_radius_discarded_total{reason=\"invalid\"} 1\n",
        );
        let change = after.since(&before);
        assert_eq!((change.packets.auth, change.packets.acct, change.duplicates), (4, 2, 3));
        assert_eq!((change.discarded.overload, change.discarded.unknown_client, change.discarded.invalid), (7, 0, 1));
    }
}
