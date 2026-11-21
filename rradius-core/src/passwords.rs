//! Verifies a request's password against a stored password.
//!
//! Stored formats: plaintext (no prefix), `{clear}`, `{crypt}` (crypt(3), including
//! DES, `$1$` MD5-crypt, `$5$`, `$6$` and bcrypt), `{SHA}` (Base64 SHA-1), `{SSHA}`
//! (Base64 of SHA-1(password + salt) + salt), `{nthash}` (hex MD4 of UTF-16LE) and
//! `{argon2}` (a PHC string). For moving accounts from other systems, also `{MD5}`,
//! `{SMD5}`, `{SHA224}`, `{SSHA224}`, `{SHA256}`, `{SSHA256}`, `{SHA384}`, `{SSHA384}`,
//! `{SHA512}`, `{SSHA512}`, `{PBKDF2-SHA1}` (and `{PBKDF2}`, the same), `{PBKDF2-SHA256}`,
//! `{PBKDF2-SHA512}`, `{X-PBKDF2}` and `{NS-MTA-MD5}`.
use argon2::{Argon2, PasswordVerifier};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use md5::Md5;
use sha1::{Digest, Sha1};
use sha2::{Sha224, Sha256, Sha384, Sha512};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

/// Compares in time that depends only on the lengths, not on where they differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Strips a case-insensitive `{scheme}` prefix, returning (scheme, rest).
pub(crate) fn split_scheme(stored: &str) -> (Option<String>, &str) {
    if let Some(rest) = stored.strip_prefix('{') {
        if let Some(end) = rest.find('}') {
            return (Some(rest[..end].to_ascii_lowercase()), &rest[end + 1..]);
        }
    }
    (None, stored)
}

/// Whether `stored` has no `{scheme}` prefix but starts like a crypt(3), bcrypt, SHA-crypt or
/// argon2 hash. Such a value was most likely imported without its prefix, and comparing it as
/// plaintext would let whoever holds the hash log in by sending the hash itself.
/// A deliberate plaintext password that looks like this takes `{clear}`.
pub(crate) fn looks_like_unprefixed_hash(stored: &str) -> bool {
    const MARKERS: [&str; 7] = ["$2a$", "$2b$", "$2y$", "$1$", "$5$", "$6$", "$argon2"];
    split_scheme(stored).0.is_none() && MARKERS.iter().any(|marker| stored.starts_with(marker))
}

/// The formats `rradius_password_checks_total` tells apart, as its `scheme` label has them:
/// a fixed set, so that nothing the data holds becomes a label.
pub const SCHEME_LABELS: [&str; 23] = [
    "clear",
    "none",
    "crypt",
    "md5",
    "smd5",
    "sha",
    "ssha",
    "sha224",
    "ssha224",
    "sha256",
    "ssha256",
    "sha384",
    "ssha384",
    "sha512",
    "ssha512",
    "pbkdf2-sha1",
    "pbkdf2-sha256",
    "pbkdf2-sha512",
    "x-pbkdf2",
    "ns-mta-md5",
    "nthash",
    "argon2",
    "unknown",
];

/// The label of the format `stored` is in: `none` without a prefix, `unknown` for a prefix that
/// is not a format, and `pbkdf2-sha1` for both of its names.
pub fn scheme_label(stored: &str) -> &'static str {
    let (scheme, _) = split_scheme(stored);
    let Some(scheme) = scheme else { return "none" };
    let scheme = if scheme == "pbkdf2" { "pbkdf2-sha1" } else { scheme.as_str() };
    SCHEME_LABELS.iter().copied().find(|label| *label == scheme && *label != "none" && *label != "unknown").unwrap_or("unknown")
}

/// Comparisons of a login against a stored password, by format and result: two places for each
/// of `SCHEME_LABELS`, the failed ones first.
static CHECKS: [std::sync::atomic::AtomicU64; SCHEME_LABELS.len() * 2] = [const { std::sync::atomic::AtomicU64::new(0) }; SCHEME_LABELS.len() * 2];

/// Counts one comparison against a value in the format `label`, which `scheme_label` gave.
pub fn record_check(label: &'static str, ok: bool) {
    let Some(index) = SCHEME_LABELS.iter().position(|known| *known == label) else { return };
    CHECKS[index * 2 + usize::from(ok)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many comparisons have been counted for `label` with this result.
#[cfg(test)]
pub(crate) fn checks_counted(label: &str, ok: bool) -> u64 {
    SCHEME_LABELS.iter().position(|known| *known == label).map_or(0, |index| CHECKS[index * 2 + usize::from(ok)].load(std::sync::atomic::Ordering::Relaxed))
}

/// Writes `rradius_password_checks_total` in the Prometheus text format, every combination.
pub fn render_checks(out: &mut String) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP rradius_password_checks_total Logins compared with a stored password, by the format of the stored value and the result.\n# TYPE rradius_password_checks_total counter");
    for (index, label) in SCHEME_LABELS.iter().enumerate() {
        for (offset, result) in ["failed", "ok"].into_iter().enumerate() {
            let _ = writeln!(out, "rradius_password_checks_total{{scheme=\"{label}\",result=\"{result}\"}} {}", CHECKS[index * 2 + offset].load(std::sync::atomic::Ordering::Relaxed));
        }
    }
}

/// Checks `given` against Base64 of a digest, followed by a salt of at least one octet if
/// `salted`: the digest covers the password and then the salt.
fn verify_digest<D: Digest>(value: &str, given: &[u8], salted: bool) -> bool {
    let Ok(decoded) = STANDARD.decode(value) else { return false };
    let length = <D as Digest>::output_size();
    let has_right_length = if salted { decoded.len() > length } else { decoded.len() == length };
    if !has_right_length {
        return false;
    }
    let (digest, salt) = decoded.split_at(length);
    let mut hasher = D::new();
    hasher.update(given);
    hasher.update(salt);
    constant_time_eq(&hasher.finalize(), digest)
}

/// Adapted Base64: `.` for `+`, with no padding.
fn decode_adapted_base64(value: &str) -> Option<Vec<u8>> {
    if value.contains('+') {
        return None;
    }
    STANDARD_NO_PAD.decode(value.replace('.', "+")).ok()
}

/// Checks `given` against `<iterations>$<salt>$<derived key>` as OpenLDAP's pw-pbkdf2 writes
/// it, with the salt and key in adapted Base64. The key is as long as the digest.
fn verify_pbkdf2<D>(value: &str, given: &[u8]) -> bool
where
    D: Digest + hmac::EagerHash,
    hmac::Hmac<D>: hmac::KeyInit + hmac::Mac + Clone + Sync,
{
    let mut fields = value.split('$');
    let (Some(iterations), Some(salt), Some(key), None) = (fields.next(), fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    let (Ok(iterations), Some(salt), Some(key)) = (iterations.parse::<u32>(), decode_adapted_base64(salt), decode_adapted_base64(key)) else {
        return false;
    };
    if iterations == 0 || salt.is_empty() || key.len() != <D as Digest>::output_size() {
        return false;
    }
    pbkdf2_matches::<D>(given, &salt, iterations, &key)
}

/// Whether PBKDF2 with HMAC over `D` derives `key` from `given` and `salt`.
fn pbkdf2_matches<D>(given: &[u8], salt: &[u8], iterations: u32, key: &[u8]) -> bool
where
    D: Digest + hmac::EagerHash,
    hmac::Hmac<D>: hmac::KeyInit + hmac::Mac + Clone + Sync,
{
    let mut derived = vec![0u8; key.len()];
    pbkdf2::pbkdf2_hmac::<D>(given, salt, iterations, &mut derived);
    constant_time_eq(&derived, key)
}

/// Base64 with its padding or without it, but not with padding that is wrong.
fn decode_base64_any_padding(value: &str) -> Option<Vec<u8>> {
    const ENGINE: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    ENGINE.decode(value).ok()
}

/// Checks `given` against `<algorithm>:<iterations>:<salt>:<key>`: the algorithm `HMACSHA1`
/// or `HMACSHA2+<bits>`, the iterations a 4-octet big-endian number, all in Base64, and a key
/// as long as the digest.
fn verify_x_pbkdf2(value: &str, given: &[u8]) -> bool {
    let mut fields = value.split(':');
    let (Some(algorithm), Some(iterations), Some(salt), Some(key), None) = (fields.next(), fields.next(), fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    let (Some(iterations), Some(salt), Some(key)) = (decode_base64_any_padding(iterations), decode_base64_any_padding(salt), decode_base64_any_padding(key)) else {
        return false;
    };
    let Ok(iterations) = <[u8; 4]>::try_from(iterations.as_slice()).map(u32::from_be_bytes) else { return false };
    match algorithm {
        "HMACSHA1" => x_pbkdf2_check::<Sha1>(given, &salt, iterations, &key),
        "HMACSHA2+224" => x_pbkdf2_check::<Sha224>(given, &salt, iterations, &key),
        "HMACSHA2+256" => x_pbkdf2_check::<Sha256>(given, &salt, iterations, &key),
        "HMACSHA2+384" => x_pbkdf2_check::<Sha384>(given, &salt, iterations, &key),
        "HMACSHA2+512" => x_pbkdf2_check::<Sha512>(given, &salt, iterations, &key),
        _ => false,
    }
}

/// The checks of `{X-PBKDF2}` that depend on the digest, then the derivation.
fn x_pbkdf2_check<D>(given: &[u8], salt: &[u8], iterations: u32, key: &[u8]) -> bool
where
    D: Digest + hmac::EagerHash,
    hmac::Hmac<D>: hmac::KeyInit + hmac::Mac + Clone + Sync,
{
    iterations > 0 && !salt.is_empty() && key.len() == <D as Digest>::output_size() && pbkdf2_matches::<D>(given, salt, iterations, key)
}

/// Checks `given` against 32 hexadecimal digits of MD5(salt, 0x59, password, 0xF7, salt)
/// followed by the 32-character salt, as old mail servers stored passwords.
fn verify_ns_mta_md5(value: &str, given: &[u8]) -> bool {
    let (Some(digest), Some(salt)) = (value.get(..32), value.get(32..)) else { return false };
    if salt.len() != 32 {
        return false;
    }
    // 32 hexadecimal digits and nothing else, as an NT hash is written.
    let Some(digest) = crate::mschap::parse_nt_hash(digest) else { return false };
    let mut hasher = Md5::new();
    hasher.update(salt.as_bytes());
    hasher.update([0x59]);
    hasher.update(given);
    hasher.update([0xF7]);
    hasher.update(salt.as_bytes());
    constant_time_eq(&hasher.finalize(), &*digest)
}

/// Checks a PAP password against `stored`. Unknown schemes never match, and neither does a
/// hash stored without its prefix (see `looks_like_unprefixed_hash`).
pub fn verify_pap(stored: &str, given: &[u8]) -> bool {
    if looks_like_unprefixed_hash(stored) {
        return false;
    }
    if cost_over_limit(stored).is_some() {
        return false;
    }
    let (scheme, value) = split_scheme(stored);
    match scheme.as_deref() {
        None | Some("clear") => constant_time_eq(value.as_bytes(), given),
        Some("crypt") => pwhash::unix::verify(given, value),
        Some("md5") => verify_digest::<Md5>(value, given, false),
        Some("smd5") => verify_digest::<Md5>(value, given, true),
        Some("sha") => verify_digest::<Sha1>(value, given, false),
        Some("ssha") => verify_digest::<Sha1>(value, given, true),
        Some("sha224") => verify_digest::<Sha224>(value, given, false),
        Some("ssha224") => verify_digest::<Sha224>(value, given, true),
        Some("sha384") => verify_digest::<Sha384>(value, given, false),
        Some("ssha384") => verify_digest::<Sha384>(value, given, true),
        Some("x-pbkdf2") => verify_x_pbkdf2(value, given),
        Some("ns-mta-md5") => verify_ns_mta_md5(value, given),
        Some("sha256") => verify_digest::<Sha256>(value, given, false),
        Some("ssha256") => verify_digest::<Sha256>(value, given, true),
        Some("sha512") => verify_digest::<Sha512>(value, given, false),
        Some("ssha512") => verify_digest::<Sha512>(value, given, true),
        // OpenLDAP's pw-pbkdf2 writes SHA-1 under both names.
        Some("pbkdf2-sha1" | "pbkdf2") => verify_pbkdf2::<Sha1>(value, given),
        Some("pbkdf2-sha256") => verify_pbkdf2::<Sha256>(value, given),
        Some("pbkdf2-sha512") => verify_pbkdf2::<Sha512>(value, given),
        Some("nthash") => {
            let Ok(given) = std::str::from_utf8(given) else {
                return false;
            };
            crate::mschap::parse_nt_hash(value).is_some_and(|stored| constant_time_eq(&*crate::mschap::nt_password_hash(given), &*stored))
        }
        Some("argon2") => Argon2::default().verify_password(given, value).is_ok(),
        Some(_) => false,
    }
}

/// The most a stored hash may ask of one check. A stored value carries its own cost, and
/// whoever can write it could otherwise set one that holds every slot for a long time or
/// takes all the memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashLimits {
    pub pbkdf2_iterations: u32,
    pub crypt_rounds: u32,
    pub bcrypt_cost: u32,
    pub argon2_memory_kib: u32,
    pub argon2_iterations: u32,
    pub argon2_lanes: u32,
}

impl HashLimits {
    const DEFAULT: Self = Self { pbkdf2_iterations: 5_000_000, crypt_rounds: 1_000_000, bcrypt_cost: 15, argon2_memory_kib: 262_144, argon2_iterations: 10, argon2_lanes: 16 };
}

impl Default for HashLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static LIMITS: std::sync::RwLock<HashLimits> = std::sync::RwLock::new(HashLimits::DEFAULT);

/// Sets the limits for every check from now on.
pub fn configure_limits(limits: HashLimits) {
    if let Ok(mut current) = LIMITS.write() {
        *current = limits;
    }
}

/// The limits in force.
pub fn limits() -> HashLimits {
    LIMITS.read().map_or_else(|_| HashLimits::default(), |limits| *limits)
}

/// The name of the first cost parameter in `stored` that is above its limit, as
/// `[auth.password_limits]` names it. A value whose cost cannot be read is not over a limit:
/// the check itself refuses it.
pub fn cost_over_limit(stored: &str) -> Option<&'static str> {
    cost_over(&limits(), stored)
}

fn cost_over(limits: &HashLimits, stored: &str) -> Option<&'static str> {
    let (scheme, value) = split_scheme(stored);
    match scheme.as_deref()? {
        "crypt" => {
            if let Some(rest) = value.strip_prefix("$5$").or_else(|| value.strip_prefix("$6$")) {
                let rounds = rest.strip_prefix("rounds=")?.split('$').next()?;
                return (rounds.parse::<u64>().ok()? > u64::from(limits.crypt_rounds)).then_some("crypt_rounds");
            }
            if let Some(rest) = value.strip_prefix("$sha1$") {
                let rounds = rest.split('$').next()?;
                return (rounds.parse::<u64>().ok()? > u64::from(limits.crypt_rounds)).then_some("crypt_rounds");
            }
            if let Some(rest) = value.strip_prefix('_') {
                return (bsdi_rounds(rest.get(..4)?)? > u64::from(limits.crypt_rounds)).then_some("crypt_rounds");
            }
            let rest = value.strip_prefix("$2a$").or_else(|| value.strip_prefix("$2b$")).or_else(|| value.strip_prefix("$2y$"))?;
            let cost = rest.split('$').next()?;
            (cost.parse::<u32>().ok()? > limits.bcrypt_cost).then_some("bcrypt_cost")
        }
        "pbkdf2" | "pbkdf2-sha1" | "pbkdf2-sha256" | "pbkdf2-sha512" => {
            let iterations = value.split('$').next()?.parse::<u32>().ok()?;
            (iterations > limits.pbkdf2_iterations).then_some("pbkdf2_iterations")
        }
        "x-pbkdf2" => {
            let iterations = decode_base64_any_padding(value.split(':').nth(1)?)?;
            let iterations = <[u8; 4]>::try_from(iterations.as_slice()).map(u32::from_be_bytes).ok()?;
            (iterations > limits.pbkdf2_iterations).then_some("pbkdf2_iterations")
        }
        "argon2" => {
            let hash = argon2::PasswordHash::new(value).ok()?;
            let params = argon2::Params::try_from(&hash).ok()?;
            if params.m_cost() > limits.argon2_memory_kib {
                Some("argon2_memory_kib")
            } else if params.t_cost() > limits.argon2_iterations {
                Some("argon2_iterations")
            } else if params.p_cost() > limits.argon2_lanes {
                Some("argon2_lanes")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The rounds of an extended DES hash, which `_` and four characters of `./0-9A-Za-z` give,
/// the first the least significant.
fn bsdi_rounds(encoded: &str) -> Option<u64> {
    let mut rounds = 0u64;
    for (index, c) in encoded.bytes().enumerate() {
        let digit = match c {
            b'.' => 0,
            b'/' => 1,
            b'0'..=b'9' => u64::from(c - b'0') + 2,
            b'A'..=b'Z' => u64::from(c - b'A') + 12,
            b'a'..=b'z' => u64::from(c - b'a') + 38,
            _ => return None,
        };
        rounds |= digit << (6 * index);
    }
    Some(rounds)
}

/// The schemes that are slow on purpose: they are verified away from the request threads.
fn is_slow(scheme: Option<&str>) -> bool {
    matches!(scheme, Some("crypt" | "argon2" | "pbkdf2" | "pbkdf2-sha1" | "pbkdf2-sha256" | "pbkdf2-sha512" | "x-pbkdf2"))
}

/// Like `verify_pap`, for use while handling a request. These schemes take long enough on
/// purpose to hold up a thread that serves other requests, and argon2 uses a lot of
/// memory, so they run elsewhere, with as many at once as there are CPUs.
pub async fn verify_pap_blocking(stored: &str, given: &[u8]) -> bool {
    let (scheme, _) = split_scheme(stored);
    if !is_slow(scheme.as_deref()) {
        return verify_pap(stored, given);
    }
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(Semaphore::new(std::thread::available_parallelism().map_or(1, |n| n.get()))));
    let Ok(slot) = slots.clone().acquire_owned().await else { return false };
    let (stored, given) = (stored.to_string(), zeroize::Zeroizing::new(given.to_vec()));
    // The slot goes with the work: dropping this future does not stop a running hash.
    let work = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        #[cfg(test)]
        let _running = tests::Running::new();
        verify_pap(&stored, &given)
    });
    work.await.unwrap_or(false)
}

/// Checks a CHAP response: MD5(id + password + challenge). Only plaintext or
/// `{clear}` stored passwords can be checked, since CHAP needs the password.
pub fn verify_chap(stored: &str, id: u8, response: &[u8], challenge: &[u8]) -> bool {
    if looks_like_unprefixed_hash(stored) {
        return false;
    }
    let password = match split_scheme(stored) {
        (None, value) => value,
        (Some(scheme), value) if scheme == "clear" => value,
        _ => return false,
    };
    let mut hasher = Md5::new();
    hasher.update([id]);
    hasher.update(password.as_bytes());
    hasher.update(challenge);
    constant_time_eq(&hasher.finalize(), response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static HIGHEST_RUNNING: AtomicUsize = AtomicUsize::new(0);

    /// Counts the slow hashes running while it exists.
    pub(super) struct Running;

    impl Running {
        pub(super) fn new() -> Self {
            let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
            HIGHEST_RUNNING.fetch_max(now, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_looks_like_unprefixed_hash() {
        for stored in ["$2a$10$x", "$2b$10$x", "$2y$10$x", "$2y$10$abcdefghijklmnopqrstuu", "$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/", "$6$salt$hash", "$1$salt$x", "$5$salt$x", "$6$salt$x", "$argon2id$v=19$x"] {
            assert!(looks_like_unprefixed_hash(stored), "{stored}");
        }
        for stored in ["hunter2", "$dollar", "{clear}$2y$10$x", "{crypt}$2y$10$x", "", "$2y", "$2Y$10$x"] {
            assert!(!looks_like_unprefixed_hash(stored), "{stored}");
        }
    }

    #[test]
    fn test_an_unprefixed_hash_is_not_a_password() {
        let hash = "$2y$04$i9AYrtz6ptiaUcesbfLap.sLiLXOvohQ9gYya7aS8q4sz5.UJ0FGW";
        assert!(!verify_pap(hash, hash.as_bytes()));
        assert!(!verify_pap(hash, b"password"));
        assert!(verify_pap(&format!("{{clear}}{hash}"), hash.as_bytes()));
        assert!(verify_pap("hunter2", b"hunter2"));
        let md5 = |password: &str| {
            let mut hasher = Md5::new();
            hasher.update([7]);
            hasher.update(password.as_bytes());
            hasher.update(b"challenge");
            hasher.finalize()
        };
        assert!(!verify_chap(hash, 7, &md5(hash), b"challenge"));
        assert!(verify_chap(&format!("{{clear}}{hash}"), 7, &md5(hash), b"challenge"));
        assert!(verify_chap("hunter2", 7, &md5("hunter2"), b"challenge"));
    }

    // Generated with `openssl passwd` and Python hashlib for the password "password".
    const VECTORS: &[&str] = &[
        "{crypt}abJnggxhB/yWI",
        "{crypt}$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/",
        "{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=",
        "{SSHA}yI6cZwQadOA1e+/f+T+H3eCQQhRzYWx0",
        "{nthash}8846F7EAEE8FB117AD06BDD830B7586C",
        "{nthash}8846f7eaee8fb117ad06bdd830b7586c",
        // SHA-crypt from a script written to Drepper's specification, not this crate's.
        "{crypt}$5$saltsalt$gOjOtoMpVhru2uyjeJSEc/JaLQWOXMNmlOnj6T4AtC.",
        "{crypt}$6$saltsalt$qFmFH.bQmmtXzyBY0s9v7Oicd2z4XSIecDzlB5KiA2/jctKu9YterLp8wwnSq.qc.eoxqOmSuNp2xS0ktL3nh/",
        // bcrypt from `htpasswd -B`, which writes $2y$; $2b$ is the same algorithm.
        "{crypt}$2y$04$i9AYrtz6ptiaUcesbfLap.sLiLXOvohQ9gYya7aS8q4sz5.UJ0FGW",
        "{crypt}$2b$04$i9AYrtz6ptiaUcesbfLap.sLiLXOvohQ9gYya7aS8q4sz5.UJ0FGW",
        // The argon2 reference vector the argon2 crate's tests also use.
        "{argon2}$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc",
        // From `openssl dgst` and Python's hashlib, not this crate's dependencies.
        "{MD5}X03MO1qnZdYdgyfeuILPmQ==",
        "{SMD5}/b3zQZ//mL2wJBOQ9iqds3NhbHRzYWx0",
        "{SHA256}XohImNooBHFR0OVvjcYpJ3NgPQ1qq73WKhHvch0VQtg=",
        "{SSHA256}DIzeh0gCRMTRu9dAH3C3rr7fWkRT0Bp2ZdtRqvTX3XJzYWx0c2FsdA==",
        "{SHA512}sQnzu7wkTrgkQZF+0G1hi5AI3Qmzvv0bXgc5THBqi7mAsdd4Xll27ASbRt9fEyavWi6m0QP9B8lThf+rDKy8hg==",
        "{SSHA512}9ZxHVj4YomwqqFiYKcIjExMLx2ZblYfXRGc4KMqbgvHq2+HOgwiTIi+eO/Uam/8D0beDAkGpvx14+UFlfBskLnNhbHRzYWx0",
        "{ssha512}9ZxHVj4YomwqqFiYKcIjExMLx2ZblYfXRGc4KMqbgvHq2+HOgwiTIi+eO/Uam/8D0beDAkGpvx14+UFlfBskLnNhbHRzYWx0",
        // From `slappasswd` with OpenLDAP's pw-sha2 and pw-pbkdf2 modules. Debian's build for
        // SHA-256: Alpine's pw-sha2 on aarch64 gets it wrong, differing from sha256sum.
        "{SHA256}XohImNooBHFR0OVvjcYpJ3NgPQ1qq73WKhHvch0VQtg=",
        "{SSHA256}Cj28UBb5eM3dgWV6YFmVOfkfcMF+M9MSnSIv9PJYJPEzU9KWHyp49w==",
        "{SSHA512}wHP13BUW4YF/QFX9nBi7S/YiLoATtGUjWKghKfgoXnc8aZ9Tf/pgRMHDT/Dix5DV4MMpd74HP2elvRc5dn4FMgfF8uNWnbIu",
        "{SMD5}+GoL6jBU0waf6T9hSWT+KTqztQA=",
        "{PBKDF2-SHA256}10000$WWvDtZeGAjK0xyyM1L21BA$iM4Y/APbv1q5rePOALyxAUpS2YuHdpKBHBp//goHI.k",
        "{PBKDF2-SHA512}10000$HJD8YRlhStsfJveYDNnw3w$aeLsczVFOi9KkjxSygbA5XYAaXJvQ0P7SrZdc/KEZfpAAn.if.vG.XBcIbhs/KJMaKyFu45wkRxWsEEtjtuXdQ",
        // Debian bookworm's slappasswd with pw-pbkdf2 from slapd-contrib.
        "{PBKDF2-SHA1}10000$mvXAVje0oSV7IfK0PiUtOw$blbuXZsbTgZXCFBnvYkW/NvfKmk",
        "{PBKDF2}10000$KWGEt6n7Zvz4AKu8cnc4iw$QyHvpR.HgZFfp7cAWK02aCL77S0",
        "{SHA384}qLZLq9CsqRpZvbt3YbQh1PK7OCgNOnW6DyHyvrxFWD1EbFmGYMlM5oDEfRnDB4On",
        "{SSHA384}YHAuJAp00nSfhm7g+VWE1Em8sT3wbhrukbs60ft249bxxPZZgcdSNdkjag9dLn/5TpJwewoBm6c=",
        // Debian bookworm's Perl libraries for Netscape mail and for PBKDF2 in its LDAP encoding.
        "{NS-MTA-MD5}8b9a023c0dab9ffb0cb04c28762a49bdabcdefghijklmnopqrstuvwxyz012345",
        "{X-PBKDF2}HMACSHA1:AAAD6A:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=",
        "{X-PBKDF2}HMACSHA2+256:AAAD6A:9DfsezYpXzU=:mWzyPWwWsEH4niBgeYt6r8wiGAIl99zGFNn4rcN4v+M=",
        "{X-PBKDF2}HMACSHA2+512:AAAD6A:r8ziw0GsDxw=:mPKhUCQrLRGQ+/dgOkgXiuLdDEqJcSZtvFVXo8DJDCRBbH7RK47A6/ff7sjp6w5YKqP5Il4ynGrDywQJ1JTo2g==",
        "{X-PBKDF2}HMACSHA2+224:AAAD6A:b5meEuaUnqc=:IY4WYhRatnlqRKA/VUwmnEw4gbuofsjHkxiMHQ==",
        "{X-PBKDF2}HMACSHA2+384:AAAD6A:nmyFzzICrfo=:226EkMJDNWLnpvZ9V0ub8eVlMzc6rEMoxALjCXpr9SJHW8bJ7aR/QB83oaz7kv/g",
        // From Python's hashlib; OpenSSL's `kdf` gives the same SHA-512 key.
        "{PBKDF2-SHA256}1000$MDEyMzQ1Njc4OWFiY2RlZg$hRRjgXWkW8ResfIvBP99J/T4vkgEmMRV/0tJTOjR59I",
        "{PBKDF2-SHA512}1000$MDEyMzQ1Njc4OWFiY2RlZg$38DzhdBT7fPaUGBlsh42VTuuKSFAIYGZJ7l6feCDLIl.K3hdPFgxxu7xuUi4gIuH6cEIoODn18xH9Ig2ryNgUw",
        "{PBKDF2-SHA1}1000$MDEyMzQ1Njc4OWFiY2RlZg$DYW.LTZG5wxyiF/qvsh40/./hXk",
        "{pbkdf2-sha1}1000$MDEyMzQ1Njc4OWFiY2RlZg$DYW.LTZG5wxyiF/qvsh40/./hXk",
        "{SHA224}1j3JGeIB17xMglYw0s8l/ck9Sy8NRnBtKQONAQ==",
        "{SSHA224}8lGRPjD/fknpqAXHE+PHF29CttXQszdmK/2Rh3NhbHRzYWx0",
        "{NS-MTA-MD5}8B9A023C0DAB9FFB0CB04C28762A49BDabcdefghijklmnopqrstuvwxyz012345",
        "{clear}password",
        "password",
    ];

    #[test]
    fn test_known_vectors() {
        for stored in VECTORS {
            assert!(verify_pap(stored, b"password"), "{stored} should accept 'password'");
            assert!(!verify_pap(stored, b"Password"), "{stored} should reject 'Password'");
            assert!(!verify_pap(stored, b"wrong"), "{stored} should reject 'wrong'");
        }
    }

    /// Every slow format has its cost read before any hashing: a value far above the limits
    /// is refused at once, and one at its limit is left to the check.
    /// Spec Stored Hash Cost Is Capped, Scenarios: A PBKDF2 value with a huge iteration count, crypt rounds and bcrypt cost, argon2 memory.
    #[test]
    fn test_cost_over_the_limits_is_refused_before_hashing() {
        let limits = HashLimits::default();
        let started = std::time::Instant::now();
        for stored in [
            "{PBKDF2-SHA512}4294967295$MDEyMzQ1Njc4OWFiY2RlZg$iiBcqBMfZQd6taylmx35ciLJSlmD/fhYsIhplxnAvOkqbRoX4w8ohoLTVMoHNbWw97zkObuTFAFFLnjJVF3d5A",
            "{pbkdf2-sha1}4294967295$MDEyMzQ1Njc4OWFiY2RlZg$DYW.LTZG5wxyiF/qvsh40/./hXk",
            "{X-PBKDF2}HMACSHA2+256://///w:c2FsdHNhbHQ:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "{crypt}$6$rounds=999999999$salt$hash",
            "{crypt}$5$rounds=999999999$salt$hash",
            "{crypt}$2b$31$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234",
            "{crypt}$2a$31$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234",
            "{crypt}$2y$31$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234",
            "{argon2}$argon2id$v=19$m=4194304,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc",
            "{argon2}$argon2id$v=19$m=65536,t=4000000,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc",
            "{argon2}$argon2id$v=19$m=65536,t=2,p=200$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc",
        ] {
            assert!(cost_over(&limits, stored).is_some(), "{stored}");
            assert!(!verify_pap(stored, b"password"), "{stored}");
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "refused without hashing: {:?}", started.elapsed());
        let names = |stored| cost_over(&limits, stored);
        assert_eq!(names("{PBKDF2-SHA512}4294967295$a$b"), Some("pbkdf2_iterations"));
        assert_eq!(names("{crypt}$6$rounds=999999999$salt$hash"), Some("crypt_rounds"));
        assert_eq!(names("{crypt}$2b$31$x"), Some("bcrypt_cost"));
        assert_eq!(names("{argon2}$argon2id$v=19$m=4194304,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc"), Some("argon2_memory_kib"));
        assert_eq!(names("{argon2}$argon2id$v=19$m=65536,t=4000000,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc"), Some("argon2_iterations"));
        assert_eq!(names("{argon2}$argon2id$v=19$m=65536,t=2,p=200$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc"), Some("argon2_lanes"));
    }

    /// The other `{crypt}` forms that carry a cost: `$sha1$` and the extended DES form.
    #[test]
    #[allow(deprecated)]
    fn test_sha1_crypt_and_extended_des_costs_are_limited() -> Result<(), String> {
        let limits = HashLimits { crypt_rounds: 5000, ..HashLimits::default() };
        let sha1 = |rounds| pwhash::sha1_crypt::hash_with(pwhash::HashSetup { salt: Some("saltsalt"), rounds: Some(rounds) }, "password").map_err(|e| e.to_string());
        assert_eq!(cost_over(&limits, &format!("{{crypt}}{}", sha1(5000)?)), None);
        assert_eq!(cost_over(&limits, &format!("{{crypt}}{}", sha1(5001)?)), Some("crypt_rounds"));
        assert_eq!(cost_over(&limits, "{crypt}$sha1$4294967295$saltsalt$x"), Some("crypt_rounds"));
        assert!(!verify_pap("{crypt}$sha1$4294967295$saltsalt$x", b"password"), "refused without hashing");
        let des = |rounds| pwhash::bsdi_crypt::hash_with(pwhash::HashSetup { salt: Some("salt"), rounds: Some(rounds) }, "password").map_err(|e| e.to_string());
        for rounds in [1, 5000, 5001, 65_536] {
            let hash = format!("{{crypt}}{}", des(rounds)?);
            let expected = (rounds > 5000).then_some("crypt_rounds");
            assert_eq!(cost_over(&limits, &hash), expected, "{rounds}: {hash}");
        }
        // The largest count, written out, not computed: `zzzz` is 2^24 - 1.
        assert_eq!(cost_over(&limits, "{crypt}_zzzzsaltxxxxxxxxxxx"), Some("crypt_rounds"));
        assert!(!verify_pap("{crypt}_zzzzsaltxxxxxxxxxxx", b"password"), "refused without hashing");
        Ok(())
    }

    /// Spec Stored Hash Cost Is Capped, Scenario: A value at its limit still verifies.
    #[test]
    fn test_cost_at_the_limit_is_verified() {
        let limits = HashLimits { pbkdf2_iterations: 1000, crypt_rounds: 5000, bcrypt_cost: 10, argon2_memory_kib: 65536, argon2_iterations: 2, argon2_lanes: 1 };
        // The published vector has 1000 iterations: exactly the limit.
        let pbkdf2 = "{pbkdf2-sha1}1000$MDEyMzQ1Njc4OWFiY2RlZg$DYW.LTZG5wxyiF/qvsh40/./hXk";
        assert_eq!(cost_over(&limits, pbkdf2), None);
        assert_eq!(cost_over(&HashLimits { pbkdf2_iterations: 999, ..limits }, pbkdf2), Some("pbkdf2_iterations"));
        let argon2 = "{argon2}$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc";
        assert_eq!(cost_over(&limits, argon2), None);
        assert!(verify_pap(argon2, b"password"), "within the defaults");
        assert_eq!(cost_over(&HashLimits { argon2_memory_kib: 65535, ..limits }, argon2), Some("argon2_memory_kib"));
        // A real SHA-256 hash with exactly the iterations of the limit still verifies.
        let mut derived = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"password", b"0123456789abcdef", 1000, &mut derived);
        let encode = |bytes: &[u8]| STANDARD_NO_PAD.encode(bytes).replace('+', ".");
        let sha256 = format!("{{PBKDF2-SHA256}}1000${}${}", encode(b"0123456789abcdef"), encode(&derived));
        assert_eq!(cost_over(&limits, &sha256), None);
        assert!(verify_pap(&sha256, b"password") && !verify_pap(&sha256, b"Password"));
        // `$6$` without `rounds=` takes the library's default, which no limit applies to; a bcrypt cost at the limit.
        assert_eq!(cost_over(&limits, "{crypt}$6$salt$hash"), None);
        assert_eq!(cost_over(&limits, "{crypt}$2b$10$x"), None);
        assert_eq!(cost_over(&limits, "{crypt}$2b$11$x"), Some("bcrypt_cost"));
        assert_eq!(cost_over(&limits, "{crypt}$6$rounds=5000$salt$hash"), None);
        assert_eq!(cost_over(&limits, "{crypt}$6$rounds=5001$salt$hash"), Some("crypt_rounds"));
    }

    /// A value whose cost cannot be read is left to the check, which refuses it; fixed-cost
    /// formats have no limit.
    #[test]
    fn test_unreadable_or_fixed_cost_is_not_over() {
        let limits = HashLimits::default();
        for stored in ["{PBKDF2}x", "{PBKDF2-SHA256}notanumber$a$b", "{X-PBKDF2}x", "{crypt}abJnggxhB/yWI", "{crypt}$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/", "{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", "{SSHA512}x", "{nthash}8846F7EAEE8FB117AD06BDD830B7586C", "{argon2}not-a-phc-string", "{clear}password", "password"] {
            assert_eq!(cost_over(&limits, stored), None, "{stored}");
        }
    }

    #[test]
    fn test_malformed_argon2_rejects() {
        assert!(!verify_pap("{argon2}not-a-phc-string", b"password"));
        assert!(!verify_pap("{argon2}", b"password"));
    }

    /// Every supported prefix has its label, whatever its case; no prefix is `none`; a prefix that
    /// is not a format is `unknown`, and the text of it never becomes a label.
    #[test]
    fn test_scheme_labels() {
        for (stored, label) in [
            ("{clear}x", "clear"),
            ("{CLEAR}x", "clear"),
            ("hunter2", "none"),
            ("{crypt}$6$s$h", "crypt"),
            ("{MD5}x", "md5"),
            ("{SMD5}x", "smd5"),
            ("{SHA}x", "sha"),
            ("{SSHA}x", "ssha"),
            ("{sha224}x", "sha224"),
            ("{SSHA224}x", "ssha224"),
            ("{SHA256}x", "sha256"),
            ("{SSHA256}x", "ssha256"),
            ("{SHA384}x", "sha384"),
            ("{SSHA384}x", "ssha384"),
            ("{SHA512}x", "sha512"),
            ("{SSHA512}x", "ssha512"),
            ("{PBKDF2-SHA1}x", "pbkdf2-sha1"),
            ("{PBKDF2}x", "pbkdf2-sha1"),
            ("{PBKDF2-SHA256}x", "pbkdf2-sha256"),
            ("{PBKDF2-SHA512}x", "pbkdf2-sha512"),
            ("{X-PBKDF2}x", "x-pbkdf2"),
            ("{NS-MTA-MD5}x", "ns-mta-md5"),
            ("{nthash}x", "nthash"),
            ("{argon2}x", "argon2"),
            ("{weird}x", "unknown"),
            ("{none}x", "unknown"),
            ("{unknown}x", "unknown"),
            ("{", "none"),
            ("{}x", "unknown"),
        ] {
            assert_eq!(scheme_label(stored), label, "{stored}");
        }
        // Every label a stored value can get is in the fixed set.
        assert!(SCHEME_LABELS.contains(&scheme_label("{anything at all}x")));
        assert_eq!(SCHEME_LABELS.len(), 23);
    }

    /// The text has every combination, in the fixed order, with what was counted.
    #[test]
    fn test_checks_are_rendered_for_every_combination() {
        let (ok_before, failed_before) = (checks_counted("md5", true), checks_counted("md5", false));
        record_check("md5", true);
        record_check("md5", true);
        record_check("md5", false);
        record_check("not a label", true);
        let mut out = String::new();
        render_checks(&mut out);
        let lines: Vec<&str> = out.lines().filter(|line| line.starts_with("rradius_password_checks_total{")).collect();
        assert_eq!(lines.len(), SCHEME_LABELS.len() * 2);
        for label in SCHEME_LABELS {
            for result in ["failed", "ok"] {
                assert!(lines.iter().any(|line| line.starts_with(&format!("rradius_password_checks_total{{scheme=\"{label}\",result=\"{result}\"}} "))), "{label} {result}");
            }
        }
        // Other tests count md5 logins too, so at least what was counted here.
        assert!(checks_counted("md5", true) >= ok_before + 2 && checks_counted("md5", false) > failed_before);
        assert!(!out.contains("not a label"));
    }

    #[tokio::test]
    async fn test_pbkdf2_sha1_is_verified_elsewhere() {
        assert!(is_slow(Some("pbkdf2-sha1")) && is_slow(Some("pbkdf2")) && is_slow(Some("x-pbkdf2")));
        const X_STORED: &str = "{X-PBKDF2}HMACSHA1:AAAD6A:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=";
        assert!(verify_pap_blocking(X_STORED, b"password").await);
        assert!(!verify_pap_blocking(X_STORED, b"wrong").await);
        const STORED: &str = "{PBKDF2-SHA1}10000$mvXAVje0oSV7IfK0PiUtOw$blbuXZsbTgZXCFBnvYkW/NvfKmk";
        assert!(verify_pap_blocking(STORED, b"password").await);
        assert!(!verify_pap_blocking(STORED, b"wrong").await);
    }

    #[tokio::test]
    async fn test_verify_pap_blocking_with_a_fast_scheme() {
        assert!(verify_pap_blocking("{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", b"password").await);
        assert!(!verify_pap_blocking("{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", b"Password").await);
    }

    /// One thread runs the tasks, so a hash computed on it would stop the ticker.
    #[tokio::test(flavor = "current_thread")]
    async fn test_slow_hashes_do_not_block_requests() {
        const STORED: &str = "{crypt}$2y$04$i9AYrtz6ptiaUcesbfLap.sLiLXOvohQ9gYya7aS8q4sz5.UJ0FGW";
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                loop {
                    ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
        };
        let limit = std::thread::available_parallelism().map_or(1, |n| n.get());
        let tasks: Vec<_> = (0..limit * 4).map(|_| tokio::spawn(verify_pap_blocking(STORED, b"password"))).collect();
        for task in tasks {
            assert!(task.await.unwrap());
        }
        ticker.abort();
        assert!(ticks.load(std::sync::atomic::Ordering::SeqCst) > 1, "the thread handling requests was blocked");
        let highest = HIGHEST_RUNNING.load(std::sync::atomic::Ordering::SeqCst);
        assert!((1..=limit).contains(&highest), "{highest} ran at once, the limit is {limit}");
    }

    #[test]
    fn test_empty_password_never_matches_legacy_formats() {
        for stored in VECTORS.iter().filter(|stored| stored.starts_with("{MD5") || stored.starts_with("{S") || stored.starts_with("{s") || stored.starts_with("{P") || stored.starts_with("{N") || stored.starts_with("{X")) {
            assert!(!verify_pap(stored, b""), "{stored}");
        }
    }

    #[test]
    fn test_malformed_legacy_values_never_match() {
        let thirty_two_octets = STANDARD.encode([7u8; 32]);
        for stored in [
            "{SSHA512}".to_string(),
            "{SSHA512}not-base64!".to_string(),
            "{SHA256}AAAA".to_string(),
            "{MD5}AAAA".to_string(),
            format!("{{SSHA256}}{thirty_two_octets}"),
            "{PBKDF2-SHA512}0$c2FsdA$AAAA".to_string(),
            // Zero iterations with a key of the right length, so that nothing else refuses it.
            format!("{{PBKDF2-SHA512}}0$c2FsdA${}", STANDARD_NO_PAD.encode([7u8; 64])),
            // `+` is not in adapted Base64, whose `.` stands for it.
            "{PBKDF2-SHA512}1000$MDEyMzQ1Njc4OWFiY2RlZg$38DzhdBT7fPaUGBlsh42VTuuKSFAIYGZJ7l6feCDLIl+K3hdPFgxxu7xuUi4gIuH6cEIoODn18xH9Ig2ryNgUw".to_string(),
            "{PBKDF2-SHA512}abc$c2FsdA$AAAA".to_string(),
            "{PBKDF2-SHA512}10000$c2FsdA".to_string(),
            // A SHA-256 key of 32 octets under the SHA-1 name, which needs 20.
            "{PBKDF2-SHA1}10000$c2FsdA$iM4Y/APbv1q5rePOALyxAUpS2YuHdpKBHBp//goHI.k".to_string(),
            "{PBKDF2}10000$c2FsdA$iM4Y/APbv1q5rePOALyxAUpS2YuHdpKBHBp//goHI.k".to_string(),
            "{NS-MTA-MD5}8b9a023c0dab9ffb0cb04c28762a49bd".to_string(),
            "{NS-MTA-MD5}zz9a023c0dab9ffb0cb04c28762a49bdabcdefghijklmnopqrstuvwxyz012345".to_string(),
            "{NS-MTA-MD5}8b9a023c0dab9ffb0cb04c28762a49bdabcdefghijklmnopqrstuvwxyz01234".to_string(),
            // `+` before a digit would parse as a number, but is not hexadecimal.
            "{NS-MTA-MD5}+b9a023c0dab9ffb0cb04c28762a49bdabcdefghijklmnopqrstuvwxyz012345".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAD6A=====:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAD6A:HESTKz6siz0===:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACMD5:AAAD6A:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAAAA:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA1:A+g:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA2+256:AAAD6A:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAD6A::AR/d4f/p3lGkkO7rqW2/Urr5Mlc=".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAD6A:HESTKz6siz0=".to_string(),
            "{X-PBKDF2}HMACSHA1:AAAD6A:HESTKz6siz0=:AR/d4f/p3lGkkO7rqW2/Urr5Mlc=:x".to_string(),
            "{PBKDF2-SHA512}99999999999$c2FsdA$AAAA".to_string(),
            "{PBKDF2-SHA512}1000$$38DzhdBT7fPaUGBlsh42VTuuKSFAIYGZJ7l6feCDLIl.K3hdPFgxxu7xuUi4gIuH6cEIoODn18xH9Ig2ryNgUw".to_string(),
            "{PBKDF2-SHA512}1000$c2FsdA$".to_string(),
            "{PBKDF2-SHA512}1000$c2FsdA$AAAA".to_string(),
            "{PBKDF2-SHA512}1000$!!!$AAAA".to_string(),
            "{PBKDF2-SHA512}".to_string(),
        ] {
            assert!(!verify_pap(&stored, b"password"), "{stored}");
            assert!(!verify_pap(&stored, b""), "{stored}");
        }
    }

    /// RFC 7914 section 11, a vector published for PBKDF2-HMAC-SHA-256.
    #[test]
    fn test_pbkdf2_published_vector() {
        let digest: Vec<u8> = (0..64)
            .map(|i| u8::from_str_radix(&"55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        // Only the length the SHA-256 form allows: the first 32 octets of the same output.
        let adapted = |bytes: &[u8]| STANDARD.encode(bytes).trim_end_matches('=').replace('+', ".");
        let stored = format!("{{PBKDF2-SHA256}}1${}${}", adapted(b"salt"), adapted(&digest[..32]));
        assert!(verify_pap(&stored, b"passwd"), "{stored}");
        assert!(!verify_pap(&stored, b"password"));
    }

    #[test]
    fn test_legacy_formats_cannot_answer_other_protocols() {
        let legacy = ["{MD5}", "{SMD5}", "{SHA224}", "{SSHA224}", "{SHA256}", "{SSHA256}", "{SHA384}", "{SSHA384}", "{SHA512}", "{SSHA512}", "{PBKDF2-SHA1}", "{PBKDF2}", "{PBKDF2-SHA256}", "{PBKDF2-SHA512}", "{X-PBKDF2}", "{NS-MTA-MD5}"];
        let (peer, challenge) = ([1u8; 16], [2u8; 16]);
        for scheme in legacy {
            let stored = VECTORS.iter().find(|stored| stored.starts_with(scheme)).unwrap();
            // Responses that would be right if the stored text, with or without its scheme,
            // or the real password were treated as the plain password.
            for password in [*stored, &stored[scheme.len()..], "password"] {
                let chap = chap_response(7, password, &challenge);
                assert!(!verify_chap(stored, 7, &chap, &challenge), "{stored} as {password}");
                let response = crate::mschap::response_for(password, "alice", &peer, &challenge);
                assert!(crate::mschap::verify(stored, "alice", &peer, &challenge, &response).is_none(), "{stored} as {password}");
            }
        }
    }

    /// One thread runs the tasks, so a hash computed on it would stop the ticker.
    #[tokio::test(flavor = "current_thread")]
    async fn test_pbkdf2_does_not_block_requests() {
        pbkdf2_does_not_block("{PBKDF2-SHA512}300000$MDEyMzQ1Njc4OWFiY2RlZg$iiBcqBMfZQd6taylmx35ciLJSlmD/fhYsIhplxnAvOkqbRoX4w8ohoLTVMoHNbWw97zkObuTFAFFLnjJVF3d5A").await;
        pbkdf2_does_not_block("{PBKDF2-SHA256}500000$MDEyMzQ1Njc4OWFiY2RlZg$.mtPzCjqB1uMn9RC3RkFshvq.tZ3Ho9dUVcH94nzpic").await;
    }

    async fn pbkdf2_does_not_block(stored: &str) {
        let ticks = std::sync::Arc::new(AtomicUsize::new(0));
        let ticker = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
        };
        assert!(verify_pap_blocking(stored, b"password").await);
        ticker.abort();
        assert!(ticks.load(Ordering::SeqCst) > 1, "the thread handling {stored} was blocked");
    }

    #[test]
    fn test_unknown_or_malformed_scheme_rejects() {
        assert!(!verify_pap("{rcrypt}abc", b"abc"));
        assert!(!verify_pap("{SHA}not-base64!", b"password"));
        assert!(!verify_pap("{SSHA}AAAA", b"password"));
    }

    fn chap_response(id: u8, password: &str, challenge: &[u8]) -> Vec<u8> {
        let mut hasher = Md5::new();
        hasher.update([id]);
        hasher.update(password.as_bytes());
        hasher.update(challenge);
        hasher.finalize().to_vec()
    }

    #[test]
    fn test_chap() {
        let challenge = [0x11u8; 16];
        let response = chap_response(7, "secret", &challenge);
        assert!(verify_chap("secret", 7, &response, &challenge));
        assert!(verify_chap("{clear}secret", 7, &response, &challenge));
        assert!(!verify_chap("secret", 8, &response, &challenge));
        assert!(!verify_chap("other", 7, &response, &challenge));
        assert!(!verify_chap("{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", 7, &chap_response(7, "password", &challenge), &challenge));
    }
}
