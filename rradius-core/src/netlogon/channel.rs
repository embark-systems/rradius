//! Setting up a Netlogon secure channel (MS-NRPC 3.1.4.1: `NetrServerReqChallenge`, then
//! `NetrServerAuthenticate3`) and calling Netlogon over RPC connections sealed with it
//! (the Netlogon security provider, MS-NRPC 3.3).
use super::crypto::{self, SealError, SIGNATURE_LEN};
use super::ndr::{Reader, Writer};
use super::rpc::{self, Auth, RpcError};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

pub const OPNUM_REQ_CHALLENGE: u16 = 4;
pub const OPNUM_AUTHENTICATE3: u16 = 26;
/// The Netlogon security provider, and the privacy level: every call is sealed.
pub const AUTH_TYPE_NETLOGON: u8 = 0x44;
pub const AUTH_LEVEL_PRIVACY: u8 = 6;
const AUTH_CONTEXT_ID: u32 = 1;
const WORKSTATION_SECURE_CHANNEL: u16 = 2;

/// Negotiate flags (MS-NRPC 3.1.4.2), by the letters of its table.
pub mod flags {
    /// G: validation level 3, with extra SIDs.
    pub const MULTIPLE_SIDS: u32 = 0x0000_0040;
    /// L: concurrent calls.
    pub const CONCURRENT_RPC: u32 = 0x0000_0800;
    /// O: strong keys.
    pub const STRONG_KEYS: u32 = 0x0000_4000;
    /// P: transitive trusts, and so the transitive logon levels.
    pub const TRANSITIVE_TRUSTS: u32 = 0x0000_8000;
    /// W: AES and SHA2.
    pub const SUPPORTS_AES: u32 = 0x0100_0000;
    /// Y: secure RPC.
    pub const AUTHENTICATED_RPC: u32 = 0x4000_0000;
    /// What this client offers.
    pub const OFFERED: u32 = MULTIPLE_SIDS | CONCURRENT_RPC | STRONG_KEYS | TRANSITIVE_TRUSTS | SUPPORTS_AES | AUTHENTICATED_RPC;
    /// What it cannot do without.
    pub const REQUIRED: u32 = STRONG_KEYS | SUPPORTS_AES | AUTHENTICATED_RPC;
}

/// NTSTATUS values this client tells apart.
pub mod status {
    pub const SUCCESS: u32 = 0;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const NO_TRUST_SAM_ACCOUNT: u32 = 0xC000_018B;
}

/// The computer account the channel is set up as.
pub struct Machine {
    /// The NetBIOS domain name.
    pub domain: String,
    /// Without the trailing `$`.
    pub computer: String,
    /// The NT hash of its password.
    pub nt_hash: Zeroizing<[u8; 16]>,
}

/// What the domain controller and this client agreed on.
pub struct Session {
    pub key: Zeroizing<[u8; 16]>,
    pub flags: u32,
}

/// Why a channel could not be set up or used.
#[derive(Debug)]
pub enum ChannelError {
    Rpc(RpcError),
    /// The domain controller refused, with this NTSTATUS.
    Refused(u32),
    /// It does not do what this client requires; the flags it agreed to.
    Unsupported(u32),
    /// Its server credential is not the one the session key gives.
    ServerCredential,
    Seal(SealError),
    /// No randomness for a challenge or confounder.
    Random(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Refused(status::ACCESS_DENIED) => f.write_str("the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)"),
            Self::Refused(status::NO_TRUST_SAM_ACCOUNT) => f.write_str("the domain controller has no such computer account (STATUS_NO_TRUST_SAM_ACCOUNT)"),
            Self::Refused(code) => write!(f, "the domain controller refused the secure channel (NTSTATUS 0x{code:08x})"),
            Self::Unsupported(agreed) => write!(f, "the domain controller does not agree to AES secure RPC (negotiated flags 0x{agreed:08x})"),
            Self::ServerCredential => f.write_str("the domain controller's server credential does not verify"),
            Self::Seal(e) => write!(f, "{e}"),
            Self::Random(e) => write!(f, "no random values: {e}"),
        }
    }
}

impl From<RpcError> for ChannelError {
    fn from(e: RpcError) -> Self {
        Self::Rpc(e)
    }
}

impl From<std::io::Error> for ChannelError {
    fn from(e: std::io::Error) -> Self {
        Self::Rpc(RpcError::Io(e))
    }
}

impl From<super::ndr::NdrError> for ChannelError {
    fn from(e: super::ndr::NdrError) -> Self {
        Self::Rpc(e.into())
    }
}

pub(super) fn random<const N: usize>() -> Result<[u8; N], ChannelError> {
    let mut value = [0u8; N];
    getrandom::fill(&mut value).map_err(|e| ChannelError::Random(e.to_string()))?;
    Ok(value)
}

/// `\\` and the server's name, as Netlogon calls name the server they are made to.
fn server_name(server: &str) -> String {
    format!("\\\\{server}")
}

/// The stub of `NetrServerReqChallenge`.
pub fn req_challenge(server: &str, computer: &str, client_challenge: &[u8; 8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.pointer();
    writer.wide_string(&server_name(server));
    writer.wide_string(computer);
    writer.bytes(client_challenge);
    writer.into_bytes()
}

/// The stub of `NetrServerAuthenticate3`.
pub fn authenticate3(server: &str, machine: &Machine, client_credential: &[u8; 8], offered: u32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.pointer();
    writer.wide_string(&server_name(server));
    writer.wide_string(&format!("{}$", machine.computer));
    writer.u16(WORKSTATION_SECURE_CHANNEL);
    writer.wide_string(&machine.computer);
    writer.bytes(client_credential);
    writer.u32(offered);
    writer.into_bytes()
}

/// One call on a connection that is not sealed, during setup.
async fn plain_call(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), call_id: u32, opnum: u16, stub: &[u8]) -> Result<Vec<u8>, RpcError> {
    stream.write_all(&rpc::request(call_id, opnum, stub, None, |_| Vec::new())?).await?;
    let pdu = rpc::parse(&rpc::read_pdu(stream).await?)?;
    if pdu.flags & rpc::PFC_LAST_FRAG == 0 {
        return Err(RpcError::Malformed("a setup answer in more than one fragment".to_string()));
    }
    Ok(rpc::response_stub(&pdu, call_id)?.to_vec())
}

/// Sets up a secure channel over `stream`, a fresh connection to the Netlogon port of `server`.
pub async fn authenticate(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), server: &str, machine: &Machine) -> Result<Session, ChannelError> {
    stream.write_all(&rpc::bind(1, rpc::NETLOGON, None)).await?;
    rpc::check_bind_ack(&rpc::parse(&rpc::read_pdu(stream).await?)?, 1)?;
    let client_challenge = random::<8>()?;
    let answer = plain_call(stream, 2, OPNUM_REQ_CHALLENGE, &req_challenge(server, &machine.computer, &client_challenge)).await?;
    let mut reader = Reader::new(&answer);
    let server_challenge: [u8; 8] = reader.array()?;
    let code = reader.u32()?;
    if code != status::SUCCESS {
        return Err(ChannelError::Refused(code));
    }
    let key = crypto::session_key(&machine.nt_hash, &client_challenge, &server_challenge);
    let client_credential = crypto::credential(&key, &client_challenge);
    let answer = plain_call(stream, 3, OPNUM_AUTHENTICATE3, &authenticate3(server, machine, &client_credential, flags::OFFERED)).await?;
    let mut reader = Reader::new(&answer);
    let server_credential: [u8; 8] = reader.array()?;
    let agreed = reader.u32()?;
    let _account_rid = reader.u32()?;
    let code = reader.u32()?;
    if code != status::SUCCESS {
        return Err(ChannelError::Refused(code));
    }
    if agreed & flags::REQUIRED != flags::REQUIRED {
        return Err(ChannelError::Unsupported(agreed));
    }
    if !crate::passwords::constant_time_eq(&crypto::credential(&key, &server_challenge), &server_credential) {
        return Err(ChannelError::ServerCredential);
    }
    Ok(Session { key, flags: agreed })
}

/// NL_AUTH_MESSAGE (MS-NRPC 2.2.1.3.1), a negotiate request with the OEM NetBIOS domain and
/// computer names.
pub fn auth_message(machine: &Machine) -> Vec<u8> {
    let mut token = 0u32.to_le_bytes().to_vec();
    token.extend_from_slice(&3u32.to_le_bytes());
    for name in [&machine.domain, &machine.computer] {
        token.extend_from_slice(name.as_bytes());
        token.push(0);
    }
    token
}

const NEGOTIATE_RESPONSE: u32 = 1;

/// A connection bound with the Netlogon security provider: every call is sealed with the
/// session key. One sequence number counts every sealed message on the connection, in both
/// directions: an answer carries the number after its request's.
pub struct Sealed<S> {
    stream: S,
    session: std::sync::Arc<Session>,
    call_id: u32,
    sequence: u64,
}

/// The most a reassembled answer may hold.
const MAX_ANSWER: usize = 256 * 1024;

impl<S: AsyncRead + AsyncWrite + Unpin> Sealed<S> {
    /// Binds `stream`, a fresh connection to the Netlogon port, with the secure channel.
    pub async fn bind(mut stream: S, session: std::sync::Arc<Session>, machine: &Machine) -> Result<Self, ChannelError> {
        let auth = Auth { auth_type: AUTH_TYPE_NETLOGON, level: AUTH_LEVEL_PRIVACY, context_id: AUTH_CONTEXT_ID };
        stream.write_all(&rpc::bind(1, rpc::NETLOGON, Some((auth, &auth_message(machine))))).await?;
        let token = rpc::check_bind_ack(&rpc::parse(&rpc::read_pdu(&mut stream).await?)?, 1)?;
        let accepted = token.as_deref().and_then(|token| token.get(..4)).is_some_and(|kind| u32::from_le_bytes([kind[0], kind[1], kind[2], kind[3]]) == NEGOTIATE_RESPONSE);
        if !accepted {
            return Err(RpcError::BindRefused("no Netlogon negotiate response".to_string()).into());
        }
        Ok(Self { stream, session, call_id: 1, sequence: 0 })
    }

    /// The secure channel the calls are sealed with.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Calls `opnum` with `stub`, sealed, and returns the answer's stub.
    pub async fn call(&mut self, opnum: u16, stub: &[u8]) -> Result<Zeroizing<Vec<u8>>, ChannelError> {
        self.call_id = self.call_id.wrapping_add(1);
        let call_id = self.call_id;
        let auth = Auth { auth_type: AUTH_TYPE_NETLOGON, level: AUTH_LEVEL_PRIVACY, context_id: AUTH_CONTEXT_ID };
        let confounder = random::<8>()?;
        let (key, sequence) = (&self.session.key, self.sequence);
        let pdu = rpc::request(call_id, opnum, stub, Some((auth, SIGNATURE_LEN)), |data| crypto::seal(key, sequence, true, confounder, data).to_vec())?;
        self.sequence += 1;
        self.stream.write_all(&pdu).await?;
        let mut answer = Zeroizing::new(Vec::new());
        loop {
            let pdu = rpc::parse(&rpc::read_pdu(&mut self.stream).await?)?;
            if pdu.ptype == rpc::PTYPE_FAULT {
                rpc::response_stub(&pdu, call_id)?;
            }
            let Some((trailer, pad, signature)) = &pdu.auth else {
                return Err(RpcError::Malformed("an answer that is not sealed".to_string()).into());
            };
            if trailer.auth_type != AUTH_TYPE_NETLOGON || trailer.level != AUTH_LEVEL_PRIVACY {
                return Err(RpcError::Malformed("an answer sealed some other way".to_string()).into());
            }
            let mut data = Zeroizing::new(rpc::response_stub(&pdu, call_id)?.to_vec());
            crypto::unseal(&self.session.key, self.sequence, false, signature, &mut data).map_err(ChannelError::Seal)?;
            self.sequence += 1;
            let Some(length) = data.len().checked_sub(usize::from(*pad)) else {
                return Err(RpcError::Malformed("a pad longer than the stub".to_string()).into());
            };
            if answer.len() + length > MAX_ANSWER {
                return Err(RpcError::Malformed("an answer too long".to_string()).into());
            }
            answer.extend_from_slice(&data[..length]);
            if pdu.flags & rpc::PFC_LAST_FRAG != 0 {
                return Ok(answer);
            }
        }
    }
}

/// A domain controller for tests: it serves `NetrServerReqChallenge`, `NetrServerAuthenticate3`
/// and sealed calls, answering these with `answer`.
#[cfg(test)]
pub mod test_dc {
    use super::*;
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};

    /// How the test domain controller behaves.
    #[derive(Clone)]
    pub struct Behaviour {
        /// The computer account's NT hash it knows.
        pub nt_hash: [u8; 16],
        /// The flags it agrees to, before the AND with the client's.
        pub flags: u32,
        /// Sends a server credential computed wrongly.
        pub wrong_server_credential: bool,
    }

    impl Behaviour {
        pub fn new(password: &str) -> Self {
            Self { nt_hash: *crate::mschap::nt_password_hash(password), flags: 0x6127_ffff, wrong_server_credential: false }
        }
    }

    /// What the test domain controller keeps of the channel it set up last.
    #[derive(Default)]
    pub struct State {
        pub session: Option<Arc<Session>>,
        /// Every sealed call: its opnum and stub.
        pub calls: Vec<(u16, Vec<u8>)>,
    }

    /// The answer to a sealed call, given the channel, its opnum and its stub.
    pub type Answer = Arc<dyn Fn(&Session, u16, &[u8]) -> Vec<u8> + Send + Sync>;

    /// Listens on a local port and serves each connection; returns the port and the state.
    pub async fn start(behaviour: Behaviour, answer: Answer) -> std::io::Result<(u16, Arc<parking_lot::Mutex<State>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(parking_lot::Mutex::new(State::default()));
        let served = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (behaviour, answer, state) = (behaviour.clone(), answer.clone(), served.clone());
                tokio::spawn(async move {
                    let _ = serve(stream, behaviour, answer, state).await;
                });
            }
        });
        Ok((port, state))
    }

    async fn serve(mut stream: TcpStream, behaviour: Behaviour, answer: Answer, state: Arc<parking_lot::Mutex<State>>) -> Result<(), ChannelError> {
        let bind = rpc::parse(&rpc::read_pdu(&mut stream).await?)?;
        let auth = bind.auth.as_ref().map(|(auth, _, _)| *auth);
        let reply_token = NEGOTIATE_RESPONSE.to_le_bytes().iter().chain(&[0; 8]).copied().collect::<Vec<u8>>();
        stream.write_all(&rpc::server::bind_ack(bind.call_id, auth.map(|auth| (auth, &reply_token[..])))).await?;
        if let Some(auth) = auth {
            let Some(session) = state.lock().session.clone() else {
                return Ok(());
            };
            let mut sequence = 0u64;
            loop {
                let pdu = rpc::parse(&rpc::read_pdu(&mut stream).await?)?;
                let (_, pad, signature) = pdu.auth.clone().ok_or(RpcError::Malformed("not sealed".to_string()))?;
                let (opnum, mut data) = rpc::server::parse_request(&pdu)?;
                crypto::unseal(&session.key, sequence, true, &signature, &mut data).map_err(ChannelError::Seal)?;
                sequence += 1;
                data.truncate(data.len() - usize::from(pad));
                state.lock().calls.push((opnum, data.clone()));
                let reply = answer(&session, opnum, &data);
                let reply_sequence = sequence;
                sequence += 1;
                let pdu = rpc::server::response(pdu.call_id, rpc::PFC_FIRST_FRAG | rpc::PFC_LAST_FRAG, &reply, Some((auth, SIGNATURE_LEN)), |data| crypto::seal(&session.key, reply_sequence, false, [7; 8], data).to_vec());
                stream.write_all(&pdu).await?;
            }
        }
        let mut challenges = None;
        loop {
            let pdu = rpc::parse(&rpc::read_pdu(&mut stream).await?)?;
            let (opnum, stub) = rpc::server::parse_request(&pdu)?;
            let mut reader = Reader::new(&stub);
            let mut writer = Writer::new();
            match opnum {
                OPNUM_REQ_CHALLENGE => {
                    reader.pointer()?;
                    let length = reader.varying(None)?;
                    reader.bytes(length as usize * 2)?;
                    let length = reader.varying(None)?;
                    reader.bytes(length as usize * 2)?;
                    let client: [u8; 8] = reader.array()?;
                    let server = [0x5a; 8];
                    challenges = Some((client, server));
                    writer.bytes(&server);
                    writer.u32(status::SUCCESS);
                }
                OPNUM_AUTHENTICATE3 => {
                    for _ in 0..2 {
                        if reader.position() == 0 {
                            reader.pointer()?;
                        }
                        let length = reader.varying(None)?;
                        reader.bytes(length as usize * 2)?;
                    }
                    reader.u16()?;
                    let length = reader.varying(None)?;
                    reader.bytes(length as usize * 2)?;
                    let credential: [u8; 8] = reader.array()?;
                    let offered = reader.u32()?;
                    let Some((client, server)) = challenges else {
                        return Err(RpcError::Malformed("no challenge".to_string()).into());
                    };
                    let key = crypto::session_key(&behaviour.nt_hash, &client, &server);
                    if crypto::credential(&key, &client) != credential {
                        writer.bytes(&[0; 8]);
                        writer.u32(0);
                        writer.u32(0);
                        writer.u32(status::ACCESS_DENIED);
                    } else {
                        let agreed = offered & behaviour.flags;
                        let mut server_credential = crypto::credential(&key, &server);
                        if behaviour.wrong_server_credential {
                            server_credential[0] ^= 1;
                        }
                        writer.bytes(&server_credential);
                        writer.u32(agreed);
                        writer.u32(1105);
                        writer.u32(status::SUCCESS);
                        state.lock().session = Some(Arc::new(Session { key, flags: agreed }));
                    }
                }
                _ => return Err(RpcError::Malformed(format!("opnum {opnum}")).into()),
            }
            stream.write_all(&rpc::server::response(pdu.call_id, rpc::PFC_FIRST_FRAG | rpc::PFC_LAST_FRAG, &writer.into_bytes(), None, |_| Vec::new())).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_dc::{self, Behaviour};
    use super::*;
    use std::sync::Arc;
    use tokio::net::TcpStream;

    fn machine(password: &str) -> Machine {
        Machine { domain: "CORP".to_string(), computer: "RRADIUS1".to_string(), nt_hash: crate::mschap::nt_password_hash(password) }
    }

    fn echo() -> test_dc::Answer {
        Arc::new(|_, opnum, stub| [opnum.to_le_bytes().as_slice(), stub].concat())
    }

    /// Scenario: Channel set up, and a sealed call over it.
    #[tokio::test]
    async fn test_channel_and_sealed_call() -> Result<(), String> {
        let (port, state) = test_dc::start(Behaviour::new("machine-pw"), echo()).await.map_err(|e| e.to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let session = Arc::new(authenticate(&mut stream, "dc1", &machine("machine-pw")).await.map_err(|e| e.to_string())?);
        assert_eq!(session.flags & flags::REQUIRED, flags::REQUIRED);
        let stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let mut sealed = Sealed::bind(stream, session, &machine("machine-pw")).await.map_err(|e| e.to_string())?;
        // Lengths that need padding, and one that does not; the sequence numbers advance.
        for stub in [vec![1u8, 2, 3], vec![9; 30], vec![4; 14]] {
            let answer = sealed.call(39, &stub).await.map_err(|e| e.to_string())?;
            assert_eq!(&answer[..], &[[39u8, 0].as_slice(), &stub].concat()[..]);
        }
        assert_eq!(state.lock().calls.len(), 3);
        Ok(())
    }

    /// Scenario: Wrong machine password.
    #[tokio::test]
    async fn test_wrong_machine_password() -> Result<(), String> {
        let (port, _) = test_dc::start(Behaviour::new("machine-pw"), echo()).await.map_err(|e| e.to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let error = authenticate(&mut stream, "dc1", &machine("other")).await.err().ok_or("accepted")?;
        assert!(matches!(error, ChannelError::Refused(status::ACCESS_DENIED)), "{error}");
        assert!(error.to_string().contains("STATUS_ACCESS_DENIED"));
        Ok(())
    }

    /// Scenario: Server credential does not verify.
    #[tokio::test]
    async fn test_server_credential_does_not_verify() -> Result<(), String> {
        let behaviour = Behaviour { wrong_server_credential: true, ..Behaviour::new("machine-pw") };
        let (port, _) = test_dc::start(behaviour, echo()).await.map_err(|e| e.to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let error = authenticate(&mut stream, "dc1", &machine("machine-pw")).await.err().ok_or("accepted")?;
        assert!(matches!(error, ChannelError::ServerCredential), "{error}");
        Ok(())
    }

    /// Scenario: No AES.
    #[tokio::test]
    async fn test_no_aes() -> Result<(), String> {
        let behaviour = Behaviour { flags: 0x6027_ffff & !flags::SUPPORTS_AES, ..Behaviour::new("machine-pw") };
        let (port, _) = test_dc::start(behaviour, echo()).await.map_err(|e| e.to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let error = authenticate(&mut stream, "dc1", &machine("machine-pw")).await.err().ok_or("accepted")?;
        assert!(matches!(error, ChannelError::Unsupported(agreed) if agreed & flags::SUPPORTS_AES == 0), "{error}");
        Ok(())
    }

    #[test]
    fn test_setup_stubs() {
        let stub = req_challenge("dc1", "RRADIUS1", &[1; 8]);
        // Pointer, then "\\dc1" with its NUL (6 units), then "RRADIUS1" (9 units), then the challenge.
        assert_eq!(stub.len(), 4 + 12 + 12 + 12 + 18 + 8);
        assert_eq!(&stub[stub.len() - 8..], &[1; 8]);
        assert_eq!(auth_message(&machine("x")), [&[0u8, 0, 0, 0, 3, 0, 0, 0][..], b"CORP\0RRADIUS1\0"].concat());
    }
}
