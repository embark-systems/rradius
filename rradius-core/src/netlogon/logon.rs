//! Network logons with `NetrLogonSamLogonEx` (MS-NRPC 3.5.4.5.1) over a sealed connection, and
//! the validation information that comes back (MS-NRPC 2.2.1.4.11 and 2.2.1.4.13).
use super::channel::{self, flags, ChannelError, Sealed};
use super::crypto;
use super::ndr::{NdrError, Reader, Writer};
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

pub const OPNUM_SAM_LOGON_EX: u16 = 39;
/// MSV1_0_ALLOW_MSVCHAPV2: the challenge is an MS-CHAPv2 challenge hash.
const ALLOW_MSVCHAPV2: u32 = 0x0001_0000;
/// MSV1_0_DONT_TRY_GUEST_ACCOUNT.
const DONT_TRY_GUEST_ACCOUNT: u32 = 0x0000_0010;
pub const PARAMETER_CONTROL: u32 = ALLOW_MSVCHAPV2 | DONT_TRY_GUEST_ACCOUNT;
const NETWORK_INFORMATION: u16 = 2;
const NETWORK_TRANSITIVE_INFORMATION: u16 = 6;
const VALIDATION_SAM_INFO: u16 = 2;
const VALIDATION_SAM_INFO2: u16 = 3;
/// NETLOGON_GUEST in the user flags: the domain controller logged on the guest account.
pub const USER_FLAG_GUEST: u32 = 0x0000_0001;
/// The most sub-authorities a SID has.
const MAX_SUB_AUTHORITIES: u8 = 15;

/// NTSTATUS values of a logon.
pub mod status {
    pub const SUCCESS: u32 = 0;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const NO_SUCH_USER: u32 = 0xC000_0064;
    pub const WRONG_PASSWORD: u32 = 0xC000_006A;
    pub const INVALID_LOGON_HOURS: u32 = 0xC000_006F;
    pub const INVALID_WORKSTATION: u32 = 0xC000_0070;
    pub const PASSWORD_EXPIRED: u32 = 0xC000_0071;
    pub const ACCOUNT_DISABLED: u32 = 0xC000_0072;
    pub const ACCOUNT_EXPIRED: u32 = 0xC000_0193;
    pub const PASSWORD_MUST_CHANGE: u32 = 0xC000_0224;
    pub const ACCOUNT_LOCKED_OUT: u32 = 0xC000_0234;

    /// The name of a status a logon can end with, for the log.
    pub fn name(status: u32) -> Option<&'static str> {
        Some(match status {
            SUCCESS => "STATUS_SUCCESS",
            ACCESS_DENIED => "STATUS_ACCESS_DENIED",
            NO_SUCH_USER => "STATUS_NO_SUCH_USER",
            WRONG_PASSWORD => "STATUS_WRONG_PASSWORD",
            INVALID_LOGON_HOURS => "STATUS_INVALID_LOGON_HOURS",
            INVALID_WORKSTATION => "STATUS_INVALID_WORKSTATION",
            PASSWORD_EXPIRED => "STATUS_PASSWORD_EXPIRED",
            ACCOUNT_DISABLED => "STATUS_ACCOUNT_DISABLED",
            ACCOUNT_EXPIRED => "STATUS_ACCOUNT_EXPIRED",
            PASSWORD_MUST_CHANGE => "STATUS_PASSWORD_MUST_CHANGE",
            ACCOUNT_LOCKED_OUT => "STATUS_ACCOUNT_LOCKED_OUT",
            _ => return None,
        })
    }
}

/// One MS-CHAPv2 response to check.
pub struct Request<'a> {
    /// The domain controller's name, without `\\`.
    pub server: &'a str,
    /// This computer's account name, without `$`.
    pub computer: &'a str,
    /// The user's domain, as the logon names it.
    pub domain: &'a str,
    pub account: &'a str,
    pub workstation: &'a str,
    /// The challenge hash of RFC 2759 section 8.2.
    pub challenge: [u8; 8],
    pub nt_response: [u8; 24],
}

/// The levels a request uses, from the flags the channel agreed on.
fn levels(agreed: u32) -> (u16, u16) {
    let logon = if agreed & flags::TRANSITIVE_TRUSTS != 0 { NETWORK_TRANSITIVE_INFORMATION } else { NETWORK_INFORMATION };
    let validation = if agreed & flags::MULTIPLE_SIDS != 0 { VALIDATION_SAM_INFO2 } else { VALIDATION_SAM_INFO };
    (logon, validation)
}

/// The stub of `NetrLogonSamLogonEx` for `request`; `logon_id` has no meaning to the
/// domain controller.
pub fn sam_logon_ex(request: &Request<'_>, agreed: u32, logon_id: u64) -> Vec<u8> {
    let (logon_level, validation_level) = levels(agreed);
    let mut writer = Writer::new();
    writer.pointer();
    writer.wide_string(&format!("\\\\{}", request.server));
    writer.pointer();
    writer.wide_string(request.computer);
    writer.u16(logon_level);
    // The union: its discriminant, then a unique pointer to NETLOGON_NETWORK_INFO.
    writer.u16(logon_level);
    writer.pointer();
    writer.unicode_string_header(request.domain);
    writer.u32(PARAMETER_CONTROL);
    writer.u32(logon_id as u32);
    writer.u32((logon_id >> 32) as u32);
    writer.unicode_string_header(request.account);
    writer.unicode_string_header(request.workstation);
    writer.bytes(&request.challenge);
    writer.octet_string_header(&request.nt_response);
    writer.octet_string_header(&[]);
    writer.unicode_chars(request.domain);
    writer.unicode_chars(request.account);
    writer.unicode_chars(request.workstation);
    writer.octet_chars(&request.nt_response);
    writer.u16(validation_level);
    // ExtraFlags.
    writer.u32(0);
    writer.into_bytes()
}

/// A security identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sid {
    pub revision: u8,
    pub authority: [u8; 6],
    pub sub_authorities: Vec<u32>,
}

impl Sid {
    /// This SID with `rid` appended, as a group of a domain.
    pub fn with_rid(&self, rid: u32) -> Self {
        let mut sid = self.clone();
        sid.sub_authorities.push(rid);
        sid
    }
}

impl std::str::FromStr for Sid {
    type Err = String;

    /// `S-<revision>-<authority>-<sub-authority>...`, with one to fifteen sub-authorities and the
    /// authority in decimal or as `0x` and twelve hexadecimal digits.
    fn from_str(text: &str) -> Result<Self, String> {
        let malformed = || format!("'{text}' is not a SID of the form S-1-<authority>-<sub-authority>...");
        let mut parts = text.split('-');
        if !parts.next().is_some_and(|s| s.eq_ignore_ascii_case("s")) {
            return Err(malformed());
        }
        let revision = parts.next().and_then(|r| r.parse::<u8>().ok()).ok_or_else(malformed)?;
        let authority = parts.next().ok_or_else(malformed)?;
        let authority = match authority.strip_prefix("0x").or_else(|| authority.strip_prefix("0X")) {
            Some(hex) if hex.len() == 12 => u64::from_str_radix(hex, 16).ok(),
            Some(_) => None,
            None => authority.parse::<u32>().ok().map(u64::from),
        }
        .ok_or_else(malformed)?;
        let sub_authorities = parts.map(|part| part.parse::<u32>().ok().filter(|_| part.bytes().all(|b| b.is_ascii_digit()))).collect::<Option<Vec<u32>>>().ok_or_else(malformed)?;
        if revision != 1 || sub_authorities.is_empty() || sub_authorities.len() > usize::from(MAX_SUB_AUTHORITIES) {
            return Err(malformed());
        }
        let mut octets = [0u8; 6];
        octets.copy_from_slice(&authority.to_be_bytes()[2..]);
        Ok(Sid { revision, authority: octets, sub_authorities })
    }
}

impl std::fmt::Display for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S-{}-", self.revision)?;
        if self.authority[..2] == [0, 0] {
            let value = u32::from_be_bytes([self.authority[2], self.authority[3], self.authority[4], self.authority[5]]);
            write!(f, "{value}")?;
        } else {
            f.write_str("0x")?;
            for byte in self.authority {
                write!(f, "{byte:02X}")?;
            }
        }
        for sub in &self.sub_authorities {
            write!(f, "-{sub}")?;
        }
        Ok(())
    }
}

/// What a successful logon says about the user.
pub struct Validation {
    pub account_name: String,
    pub logon_domain: String,
    pub user_flags: u32,
    /// Decrypted: for an MS-CHAPv2 logon, the hash of the user's NT hash.
    pub user_session_key: Zeroizing<[u8; 16]>,
    pub domain_sid: Option<Sid>,
    pub rid: u32,
    pub primary_group: u32,
    pub group_rids: Vec<u32>,
    pub extra_sids: Vec<Sid>,
}

impl Validation {
    /// Every group the user is in, as SIDs: the domain's groups and the extra SIDs.
    pub fn group_sids(&self) -> Vec<Sid> {
        let mut sids: Vec<Sid> = Vec::new();
        if let Some(domain) = &self.domain_sid {
            for rid in std::iter::once(self.primary_group).chain(self.group_rids.iter().copied()) {
                let sid = domain.with_rid(rid);
                if !sids.contains(&sid) {
                    sids.push(sid);
                }
            }
        }
        for sid in &self.extra_sids {
            if !sids.contains(sid) {
                sids.push(sid.clone());
            }
        }
        sids
    }

    pub fn is_guest(&self) -> bool {
        self.user_flags & USER_FLAG_GUEST != 0
    }
}

/// The answer to `NetrLogonSamLogonEx`.
pub struct Answer {
    pub status: u32,
    /// False when another domain controller may know better.
    pub authoritative: bool,
    /// Present when the logon succeeded.
    pub validation: Option<Validation>,
}

/// An RPC_UNICODE_STRING's length and whether its characters follow.
struct StringHeader {
    octets: u16,
    present: bool,
}

fn string_header(reader: &mut Reader<'_>) -> Result<StringHeader, NdrError> {
    let octets = reader.u16()?;
    let size = reader.u16()?;
    let present = reader.pointer()?;
    if size < octets {
        return Err(NdrError(format!("a string of {octets} octets in {size}")));
    }
    Ok(StringHeader { octets, present })
}

fn deferred_string(reader: &mut Reader<'_>, header: &StringHeader) -> Result<String, NdrError> {
    if header.present {
        reader.unicode_chars(header.octets)
    } else {
        Ok(String::new())
    }
}

fn sid(reader: &mut Reader<'_>) -> Result<Sid, NdrError> {
    let conformance = reader.count()?;
    let revision = reader.u8()?;
    let count = reader.u8()?;
    if count > MAX_SUB_AUTHORITIES || u32::from(count) != conformance {
        return Err(NdrError(format!("a SID with {count} sub-authorities")));
    }
    let authority = reader.array()?;
    let sub_authorities = (0..count).map(|_| reader.u32()).collect::<Result<_, _>>()?;
    Ok(Sid { revision, authority, sub_authorities })
}

/// NETLOGON_VALIDATION_SAM_INFO, and with `extra` the SIDs that SAM_INFO2 adds.
fn validation(reader: &mut Reader<'_>, session_key: &[u8; 16], extra: bool) -> Result<Validation, NdrError> {
    // Logon, logoff, kickoff and the three password times.
    for _ in 0..12 {
        reader.u32()?;
    }
    let names = (0..6).map(|_| string_header(reader)).collect::<Result<Vec<_>, _>>()?;
    reader.u16()?;
    reader.u16()?;
    let rid = reader.u32()?;
    let primary_group = reader.u32()?;
    let group_count = reader.u32()?;
    let groups_present = reader.pointer()?;
    let user_flags = reader.u32()?;
    let mut user_session_key = Zeroizing::new(reader.array::<16>()?);
    let logon_server = string_header(reader)?;
    let logon_domain = string_header(reader)?;
    let domain_sid_present = reader.pointer()?;
    // LMSessKey, UserAccountControl, SubAuthStatus, two logon times, the failed count and
    // a reserved word: ExpansionRoom.
    reader.bytes(8)?;
    for _ in 0..8 {
        reader.u32()?;
    }
    let (sid_count, sids_present) = if extra { (reader.u32()?, reader.pointer()?) } else { (0, false) };

    let mut texts = Vec::with_capacity(names.len());
    for header in &names {
        texts.push(deferred_string(reader, header)?);
    }
    let mut group_rids = Vec::new();
    if groups_present {
        let count = reader.count()?;
        if count != group_count {
            return Err(NdrError(format!("{count} groups where {group_count} were announced")));
        }
        for _ in 0..count {
            group_rids.push(reader.u32()?);
            reader.u32()?;
        }
    }
    deferred_string(reader, &logon_server)?;
    let logon_domain = deferred_string(reader, &logon_domain)?;
    let domain_sid = if domain_sid_present { Some(sid(reader)?) } else { None };
    let mut extra_sids = Vec::new();
    if sids_present {
        let count = reader.count()?;
        if count != sid_count {
            return Err(NdrError(format!("{count} SIDs where {sid_count} were announced")));
        }
        let mut present = Vec::with_capacity(count as usize);
        for _ in 0..count {
            present.push(reader.pointer()?);
            reader.u32()?;
        }
        for present in present {
            if present {
                extra_sids.push(sid(reader)?);
            }
        }
    }
    crypto::decrypt_validation_key(session_key, &mut user_session_key[..]);
    let account_name = texts.into_iter().next().unwrap_or_default();
    Ok(Validation { account_name, logon_domain, user_flags, user_session_key, domain_sid, rid, primary_group, group_rids, extra_sids })
}

/// Reads the answer stub, decrypting the user session key with the channel's session key.
pub fn parse_answer(stub: &[u8], session_key: &[u8; 16]) -> Result<Answer, NdrError> {
    let mut reader = Reader::new(stub);
    let level = reader.u16()?;
    let present = reader.pointer()?;
    let validation = match (present, level) {
        (false, _) => None,
        (true, VALIDATION_SAM_INFO) => Some(validation(&mut reader, session_key, false)?),
        (true, VALIDATION_SAM_INFO2) => Some(validation(&mut reader, session_key, true)?),
        (true, level) => return Err(NdrError(format!("validation level {level}"))),
    };
    let authoritative = reader.u8()? != 0;
    reader.u32()?;
    let status = reader.u32()?;
    if reader.remaining() != 0 {
        return Err(NdrError(format!("{} octets after the status", reader.remaining())));
    }
    if status == status::SUCCESS && validation.is_none() {
        return Err(NdrError("success without validation information".to_string()));
    }
    Ok(Answer { status, authoritative, validation })
}

/// Asks the domain controller on `sealed` to check `request`.
pub async fn logon<S: AsyncRead + AsyncWrite + Unpin>(sealed: &mut Sealed<S>, request: &Request<'_>) -> Result<Answer, ChannelError> {
    let logon_id = u64::from_le_bytes(channel::random::<8>()?);
    let (agreed, key) = (sealed.session().flags, sealed.session().key.clone());
    // It holds the NT-Response and the challenge hash.
    let asked = Zeroizing::new(sam_logon_ex(request, agreed, logon_id));
    let stub = sealed.call(OPNUM_SAM_LOGON_EX, &asked).await?;
    Ok(parse_answer(&stub, &key)?)
}

/// The test domain controller's side of `NetrLogonSamLogonEx`.
#[cfg(test)]
pub mod test_dc {
    use super::*;
    use crate::netlogon::channel::Session;

    /// What the request asked.
    pub struct Asked {
        pub server: String,
        pub computer: String,
        pub logon_level: u16,
        pub parameter_control: u32,
        pub domain: String,
        pub account: String,
        pub workstation: String,
        pub challenge: [u8; 8],
        pub nt_response: Vec<u8>,
        pub lm_response: Vec<u8>,
        pub validation_level: u16,
    }

    fn wide_string(reader: &mut Reader<'_>) -> Result<String, NdrError> {
        let count = reader.varying(None)?;
        let raw = reader.bytes(count as usize * 2)?;
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        Ok(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }

    fn octets(reader: &mut Reader<'_>, length: u16, present: bool) -> Result<Vec<u8>, NdrError> {
        if !present {
            return Ok(Vec::new());
        }
        let count = reader.varying(Some(u32::from(length)))?;
        Ok(reader.bytes(count as usize)?.to_vec())
    }

    pub fn parse_request(stub: &[u8]) -> Result<Asked, NdrError> {
        let mut reader = Reader::new(stub);
        reader.pointer()?;
        let server = wide_string(&mut reader)?;
        reader.pointer()?;
        let computer = wide_string(&mut reader)?;
        let logon_level = reader.u16()?;
        if reader.u16()? != logon_level || !reader.pointer()? {
            return Err(NdrError("no network information".to_string()));
        }
        let domain = string_header(&mut reader)?;
        let parameter_control = reader.u32()?;
        reader.u32()?;
        reader.u32()?;
        let account = string_header(&mut reader)?;
        let workstation = string_header(&mut reader)?;
        let challenge = reader.array()?;
        let (nt_length, _, nt_present) = (reader.u16()?, reader.u16()?, reader.pointer()?);
        let (lm_length, _, lm_present) = (reader.u16()?, reader.u16()?, reader.pointer()?);
        let domain = deferred_string(&mut reader, &domain)?;
        let account = deferred_string(&mut reader, &account)?;
        let workstation = deferred_string(&mut reader, &workstation)?;
        let nt_response = octets(&mut reader, nt_length, nt_present)?;
        let lm_response = octets(&mut reader, lm_length, lm_present)?;
        let validation_level = reader.u16()?;
        reader.u32()?;
        if reader.remaining() != 0 {
            return Err(NdrError("octets after the request".to_string()));
        }
        Ok(Asked { server, computer, logon_level, parameter_control, domain, account, workstation, challenge, nt_response, lm_response, validation_level })
    }

    /// Who the user is, for a successful answer.
    pub struct User {
        pub account: String,
        pub domain: String,
        pub flags: u32,
        pub domain_sid: Sid,
        pub rid: u32,
        pub primary_group: u32,
        pub groups: Vec<u32>,
        pub extra_sids: Vec<Sid>,
    }

    pub fn domain_sid() -> Sid {
        Sid { revision: 1, authority: [0, 0, 0, 0, 0, 5], sub_authorities: vec![21, 1004336348, 1177238915, 682003330] }
    }

    /// alice: in the domain's groups 513 and 1110, and in S-1-5-21-1-2-3-1200 elsewhere.
    pub fn user() -> User {
        User {
            account: "alice".to_string(),
            domain: "CORP".to_string(),
            flags: 0x20,
            domain_sid: domain_sid(),
            rid: 1105,
            primary_group: 513,
            groups: vec![513, 1110],
            extra_sids: vec![Sid { revision: 1, authority: [0, 0, 0, 0, 0, 5], sub_authorities: vec![21, 1, 2, 3, 1200] }],
        }
    }

    /// How the test directory answers, beyond its accounts.
    #[derive(Default)]
    pub struct Quirks {
        /// Answers the next logon with STATUS_ACCESS_DENIED, as for a channel it no longer knows.
        pub deny_once: std::sync::atomic::AtomicBool,
        /// Answers every logon as not authoritative.
        pub not_authoritative: std::sync::atomic::AtomicBool,
    }

    /// A domain controller's answers, the way Active Directory gives them: alice, User (of
    /// RFC 2759's example) and guest have `password`, and guest is logged on as the guest account; carol is disabled; anyone
    /// else does not exist.
    pub fn directory(password: &'static str, quirks: std::sync::Arc<Quirks>) -> crate::netlogon::channel::test_dc::Answer {
        use std::sync::atomic::Ordering;
        std::sync::Arc::new(move |session: &Session, opnum, stub| {
            let Ok(asked) = parse_request(stub) else {
                return Vec::new();
            };
            let level = asked.validation_level;
            let authoritative = !quirks.not_authoritative.load(Ordering::Relaxed);
            if opnum != OPNUM_SAM_LOGON_EX || quirks.deny_once.swap(false, Ordering::Relaxed) {
                return answer(session, level, status::ACCESS_DENIED, authoritative, None);
            }
            let account = asked.account.as_str();
            if account == "carol" {
                return answer(session, level, status::ACCOUNT_DISABLED, authoritative, None);
            }
            if !["alice", "User", "guest"].contains(&account) {
                return answer(session, level, status::NO_SUCH_USER, authoritative, None);
            }
            let hash = crate::mschap::nt_password_hash(password);
            let expected = crate::mschap::challenge_response(&asked.challenge, &hash);
            if expected.as_ref().map(|e| &e[..]) != Some(&asked.nt_response[..]) {
                return answer(session, level, status::WRONG_PASSWORD, authoritative, None);
            }
            let mut user = user();
            if account == "guest" {
                user.flags |= USER_FLAG_GUEST;
            }
            answer(session, level, status::SUCCESS, authoritative, Some((&user, *crate::mschap::hash_nt_password_hash(&hash))))
        })
    }

    fn write_sid(writer: &mut Writer, sid: &Sid) {
        writer.u32(sid.sub_authorities.len() as u32);
        writer.u8(sid.revision);
        writer.u8(sid.sub_authorities.len() as u8);
        writer.bytes(&sid.authority);
        for sub in &sid.sub_authorities {
            writer.u32(*sub);
        }
    }

    /// The answer stub: `user` and `user_session_key` on success, nothing on failure.
    pub fn answer(session: &Session, level: u16, status: u32, authoritative: bool, user: Option<(&User, [u8; 16])>) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u16(level);
        match user {
            None => writer.null_pointer(),
            Some((user, mut key)) => {
                writer.pointer();
                for _ in 0..12 {
                    writer.u32(0);
                }
                let names = [user.account.as_str(), "Full Name", "", "", "", ""];
                for name in names {
                    writer.unicode_string_header(name);
                }
                writer.u16(3);
                writer.u16(0);
                writer.u32(user.rid);
                writer.u32(user.primary_group);
                writer.u32(user.groups.len() as u32);
                writer.pointer();
                writer.u32(user.flags);
                crypto::encrypt_validation_key(&session.key, &mut key);
                writer.bytes(&key);
                writer.unicode_string_header("DC1");
                writer.unicode_string_header(&user.domain);
                writer.pointer();
                writer.bytes(&[0; 8]);
                for _ in 0..8 {
                    writer.u32(0);
                }
                if level == VALIDATION_SAM_INFO2 {
                    writer.u32(user.extra_sids.len() as u32);
                    if user.extra_sids.is_empty() {
                        writer.null_pointer();
                    } else {
                        writer.pointer();
                    }
                }
                for name in names {
                    writer.unicode_chars(name);
                }
                writer.u32(user.groups.len() as u32);
                for rid in &user.groups {
                    writer.u32(*rid);
                    writer.u32(7);
                }
                writer.unicode_chars("DC1");
                writer.unicode_chars(&user.domain);
                write_sid(&mut writer, &user.domain_sid);
                if level == VALIDATION_SAM_INFO2 && !user.extra_sids.is_empty() {
                    writer.u32(user.extra_sids.len() as u32);
                    for _ in &user.extra_sids {
                        writer.pointer();
                        writer.u32(7);
                    }
                    for sid in &user.extra_sids {
                        write_sid(&mut writer, sid);
                    }
                }
            }
        }
        writer.u8(u8::from(authoritative));
        writer.u32(0);
        writer.u32(status);
        writer.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_dc::{self as dc, domain_sid, user};
    use super::*;
    use crate::mschap;
    use crate::netlogon::channel::test_dc::{self, Behaviour};
    use crate::netlogon::channel::{authenticate, Machine, Session};
    use std::sync::Arc;
    use tokio::net::TcpStream;

    const PEER_CHALLENGE: [u8; 16] = [0x21; 16];
    const AUTH_CHALLENGE: [u8; 16] = [0x5B; 16];

    fn directory(password: &'static str) -> test_dc::Answer {
        dc::directory(password, Arc::new(dc::Quirks::default()))
    }

    fn machine() -> Machine {
        Machine { domain: "CORP".to_string(), computer: "RRADIUS1".to_string(), nt_hash: mschap::nt_password_hash("machine-pw") }
    }

    async fn sealed(behaviour: Behaviour, answer: test_dc::Answer) -> Result<Sealed<TcpStream>, String> {
        let (port, _) = test_dc::start(behaviour, answer).await.map_err(|e| e.to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let session = Arc::new(authenticate(&mut stream, "dc1", &machine()).await.map_err(|e| e.to_string())?);
        let stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        Sealed::bind(stream, session, &machine()).await.map_err(|e| e.to_string())
    }

    fn request<'a>(account: &'a str, password: &str) -> Request<'a> {
        Request {
            server: "dc1",
            computer: "RRADIUS1",
            domain: "CORP",
            account,
            workstation: "",
            challenge: mschap::challenge_hash(&PEER_CHALLENGE, &AUTH_CHALLENGE, account),
            nt_response: mschap::response_for(password, account, &PEER_CHALLENGE, &AUTH_CHALLENGE),
        }
    }

    /// Scenario: Correct password; the proof matches the one a stored password gives.
    #[tokio::test]
    async fn test_correct_password() -> Result<(), String> {
        let mut sealed = sealed(Behaviour::new("machine-pw"), directory("Secret1")).await?;
        let request = request("alice", "Secret1");
        let answer = logon(&mut sealed, &request).await.map_err(|e| e.to_string())?;
        assert_eq!((answer.status, answer.authoritative), (status::SUCCESS, true));
        let validation = answer.validation.ok_or("no validation")?;
        assert!(!validation.is_guest());
        assert_eq!((validation.account_name.as_str(), validation.logon_domain.as_str()), ("alice", "CORP"));
        let proof = mschap::proof_from_session_key(&validation.user_session_key, &request.nt_response, &request.challenge).ok_or("no proof")?;
        let local = mschap::verify("Secret1", "alice", &PEER_CHALLENGE, &AUTH_CHALLENGE, &request.nt_response).ok_or("local verify failed")?;
        assert_eq!(proof.authenticator_response, local.authenticator_response);
        assert_eq!((*proof.send_key, *proof.recv_key), (*local.send_key, *local.recv_key));
        let groups: Vec<String> = validation.group_sids().iter().map(Sid::to_string).collect();
        assert_eq!(
            groups,
            ["S-1-5-21-1004336348-1177238915-682003330-513", "S-1-5-21-1004336348-1177238915-682003330-1110", "S-1-5-21-1-2-3-1200"]
        );
        Ok(())
    }

    /// Scenario: Wrong password.
    #[tokio::test]
    async fn test_wrong_password() -> Result<(), String> {
        let mut sealed = sealed(Behaviour::new("machine-pw"), directory("Secret1")).await?;
        let answer = logon(&mut sealed, &request("alice", "Secret2")).await.map_err(|e| e.to_string())?;
        assert_eq!(answer.status, status::WRONG_PASSWORD);
        assert!(answer.validation.is_none());
        // The same connection serves the next logon.
        let answer = logon(&mut sealed, &request("bob", "x")).await.map_err(|e| e.to_string())?;
        assert_eq!(answer.status, status::NO_SUCH_USER);
        Ok(())
    }

    /// Guest logons are told apart, for the handler to refuse.
    #[tokio::test]
    async fn test_guest_flag() -> Result<(), String> {
        let mut sealed = sealed(Behaviour::new("machine-pw"), directory("Secret1")).await?;
        let answer = logon(&mut sealed, &request("guest", "Secret1")).await.map_err(|e| e.to_string())?;
        assert!(answer.validation.ok_or("no validation")?.is_guest());
        Ok(())
    }

    /// Without G or P the request asks for the plain levels, and SAM_INFO decodes.
    #[tokio::test]
    async fn test_plain_levels() -> Result<(), String> {
        let behaviour = Behaviour { flags: 0x6127_ffff & !(flags::MULTIPLE_SIDS | flags::TRANSITIVE_TRUSTS), ..Behaviour::new("machine-pw") };
        let mut sealed = sealed(behaviour, directory("Secret1")).await?;
        let answer = logon(&mut sealed, &request("alice", "Secret1")).await.map_err(|e| e.to_string())?;
        let validation = answer.validation.ok_or("no validation")?;
        assert!(validation.extra_sids.is_empty());
        assert_eq!(validation.group_sids().len(), 2);
        Ok(())
    }

    #[test]
    fn test_request_layout() -> Result<(), NdrError> {
        let request = request("alice", "Secret1");
        let stub = sam_logon_ex(&request, flags::OFFERED, 0x0102_0304_0506_0708);
        let asked = dc::parse_request(&stub)?;
        assert_eq!((asked.server.as_str(), asked.computer.as_str()), ("\\\\dc1", "RRADIUS1"));
        assert_eq!((asked.logon_level, asked.validation_level), (6, 3));
        assert_eq!(asked.parameter_control, 0x0001_0010);
        assert_eq!((asked.domain.as_str(), asked.account.as_str(), asked.workstation.as_str()), ("CORP", "alice", ""));
        assert_eq!((asked.challenge, &asked.nt_response[..]), (request.challenge, &request.nt_response[..]));
        assert!(asked.lm_response.is_empty());
        let asked = dc::parse_request(&sam_logon_ex(&request, flags::REQUIRED, 0))?;
        assert_eq!((asked.logon_level, asked.validation_level), (2, 2));
        Ok(())
    }

    #[test]
    fn test_answer_refuses() {
        let session = Session { key: Zeroizing::new([3; 16]), flags: flags::OFFERED };
        let good = dc::answer(&session, 3, status::SUCCESS, true, Some((&user(), [9; 16])));
        assert!(parse_answer(&good, &session.key).is_ok());
        for cut in [3, 40, 200, good.len() - 1] {
            assert!(parse_answer(&good[..cut], &session.key).is_err(), "cut at {cut}");
        }
        assert!(parse_answer(&[good.clone(), vec![0]].concat(), &session.key).is_err());
        // Success with no validation information.
        assert!(parse_answer(&dc::answer(&session, 3, status::SUCCESS, true, None), &session.key).is_err());
        // An unknown level.
        let mut other = good.clone();
        other[0] = 4;
        assert!(parse_answer(&other, &session.key).is_err());
        // A failure from a domain controller that is not authoritative.
        let answer = dc::answer(&session, 3, status::NO_SUCH_USER, false, None);
        assert!(parse_answer(&answer, &session.key).is_ok_and(|a| !a.authoritative && a.validation.is_none()));
    }

    #[test]
    fn test_sid_display() {
        assert_eq!(domain_sid().to_string(), "S-1-5-21-1004336348-1177238915-682003330");
        let odd = Sid { revision: 1, authority: [0, 1, 0, 0, 0, 0], sub_authorities: vec![] };
        assert_eq!(odd.to_string(), "S-1-0x000100000000");
    }

    #[test]
    fn test_sid_parse() -> Result<(), String> {
        let text = "S-1-5-21-1004336348-1177238915-682003330-512";
        assert_eq!(text.parse::<Sid>()?.to_string(), text);
        assert_eq!("s-1-5-32-544".parse::<Sid>()?, Sid { revision: 1, authority: [0, 0, 0, 0, 0, 5], sub_authorities: vec![32, 544] });
        assert_eq!("S-1-0x000100000000-7".parse::<Sid>()?.authority, [0, 1, 0, 0, 0, 0]);
        for bad in ["Domain Users", "S-1-5", "S-2-5-21", "S-1-x-21", "S-1-5-21-", "S-1-5-+21", "S-1-5-4294967296", "S-1-0x1-5", &format!("S-1-5{}", "-1".repeat(16))] {
            assert!(bad.parse::<Sid>().is_err(), "{bad}");
        }
        Ok(())
    }
}
