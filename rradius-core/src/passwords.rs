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

/// Checks a PAP password against `stored`. Unknown schemes never match.
pub fn verify_pap(stored: &str, given: &[u8]) -> bool {
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

    #[test]
    fn test_malformed_argon2_rejects() {
        assert!(!verify_pap("{argon2}not-a-phc-string", b"password"));
        assert!(!verify_pap("{argon2}", b"password"));
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
