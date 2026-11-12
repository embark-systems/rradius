use rradius_core::test_support::server::free_port;
use rradius_core::{
    radius::{self, RadiusServer, RadiusPacket},
    tacacs::{self, TacacsServer, TacacsPacket},
    lua_engine::{Db, LuaAuthEngine, LuaVms},
    auth_chain::{AuthChains, LuaLimits},
    config::{self, ServerConfig},
    dictionary::Dictionary,
};
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest, Md5};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::net::TcpStream;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout};

const SECRET: &str = "secret";
/// Upper bound for any single network wait, so a broken server fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(5);

/// Writes a configuration file enabling both protocols on 127.0.0.1 with one
/// client, 127.0.0.1, and a single-node cluster, and loads it the way `main`
/// does. The returned directory holds the cluster data and must outlive the test.
fn test_config() -> (tempfile::TempDir, ServerConfig) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let body = format!(
        "dictionary = \"{dictionary}\"\n[lua]\nscript = \"{script}\"\n\
         [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\nlisten = \"127.0.0.1\"\n\
         [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{raft}\"\napi = \"127.0.0.1:{api}\"\n\
         [radius]\nbind = \"127.0.0.1\"\nauth_port = 18120\nacct_port = 18130\n\
         [[radius.clients]]\nname = \"local\"\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n\
         [tacacs]\nbind = \"127.0.0.1\"\nport = 4900\n\
         [[tacacs.clients]]\nname = \"local\"\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n",
        dictionary = root.join("../share/dictionary").display(),
        script = root.join("tests/fixtures/auth.lua").display(),
        raft = free_port(),
        api = free_port(),
    );
    let path = dir.path().join("rradius.toml");
    std::fs::write(&path, body).unwrap();
    let config = config::load(&path).unwrap();
    (dir, config)
}

fn load_dictionary(config: &ServerConfig) -> Arc<Dictionary> {
    let mut dictionary = Dictionary::new();
    dictionary.load_from_file(&config.dictionary).unwrap();
    Arc::new(dictionary)
}

async fn load_engine(config: &ServerConfig) -> Arc<LuaAuthEngine> {
    let script = std::fs::read_to_string(&config.lua.script).unwrap();
    let db = Db::start(&config.cluster).await.unwrap();
    db.wait_healthy(Duration::from_secs(60)).await.unwrap();
    let vms = Arc::new(LuaVms::new(256 << 20).unwrap());
    let engine = LuaAuthEngine::new(vms, db, script).unwrap();
    Arc::new(engine.with_query_timeout(config.lua.query_timeout).with_max_execution(config.lua.max_execution))
}

/// Sends `packet` to `addr`, resending every 100 ms until a reply arrives or `deadline` passes.
/// Resending covers the window before the spawned server has bound its socket.
async fn udp_exchange(packet: &[u8], addr: &str, deadline: Duration) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 4096];
    timeout(deadline, async {
        loop {
            socket.send_to(packet, addr).await.unwrap();
            if let Ok(Ok((size, _))) = timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
                return buf[..size].to_vec();
            }
        }
    })
    .await
    .ok()
}

fn assert_response_authenticator(response: &[u8], request_authenticator: &[u8; 16]) {
    let mut for_auth = response.to_vec();
    for_auth[4..20].copy_from_slice(request_authenticator);
    let mut hasher = Md5::new();
    hasher.update(&for_auth);
    hasher.update(SECRET.as_bytes());
    assert_eq!(&response[4..20], hasher.finalize().as_slice());
}

#[tokio::test]
async fn test_full_radius_flow() {
    let (_dir, config) = test_config();
    let (engine, dictionary) = (load_engine(&config).await, load_dictionary(&config));
    let radius = config.radius.unwrap();
    let server = Arc::new(RadiusServer::new(radius.auth_addr, radius.acct_addr, radius.clients, engine, dictionary));
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // nemo's Access-Request with Message-Authenticator gets an Access-Accept carrying Filter-Id.
    let request_authenticator = [0x5au8; 16];
    let request = RadiusPacket::create_access_request(10, request_authenticator, vec![(1, b"nemo".to_vec())], SECRET.as_bytes());
    let reply = udp_exchange(&request, "127.0.0.1:18120", WAIT).await.expect("no Access-Accept within timeout");
    let res = RadiusPacket::parse(&reply).unwrap();
    assert_eq!(res.code, radius::ACCESS_ACCEPT);
    assert_eq!(res.identifier, 10);
    assert_eq!(res.attribute(11), Some(&b"std.ppp"[..]));
    assert!(res.attribute(radius::MESSAGE_AUTHENTICATOR).is_some());
    assert_response_authenticator(&reply, &request_authenticator);

    // Any other user gets an Access-Reject.
    let other = RadiusPacket::create_access_request(13, request_authenticator, vec![(1, b"mallory".to_vec())], SECRET.as_bytes());
    let reply = udp_exchange(&other, "127.0.0.1:18120", WAIT).await.expect("no Access-Reject within timeout");
    let res = RadiusPacket::parse(&reply).unwrap();
    assert_eq!(res.code, radius::ACCESS_REJECT);
    assert_eq!(res.identifier, 13);
    assert!(res.attribute(11).is_none());
    assert_response_authenticator(&reply, &request_authenticator);

    // Wrong shared secret: silently discarded.
    let bad = RadiusPacket::create_access_request(11, request_authenticator, vec![(1, b"nemo".to_vec())], b"wrong");
    assert!(udp_exchange(&bad, "127.0.0.1:18120", Duration::from_millis(500)).await.is_none());

    // The first attribute of a response is its Message-Authenticator.
    let reply = udp_exchange(&request, "127.0.0.1:18120", WAIT).await.expect("no Access-Accept within timeout");
    assert_eq!(RadiusPacket::parse(&reply).unwrap().attributes[0].0, radius::MESSAGE_AUTHENTICATOR);

    // By default a request with no Message-Authenticator is discarded.
    let bare = RadiusPacket { code: radius::ACCESS_REQUEST, identifier: 14, authenticator: request_authenticator, attributes: vec![(1, b"nemo".to_vec())] }.encode();
    assert!(udp_exchange(&bare, "127.0.0.1:18120", Duration::from_millis(500)).await.is_none());

    // Status-Server is answered on both ports, whatever the script would decide.
    let mut status = RadiusPacket { code: radius::STATUS_SERVER, identifier: 15, authenticator: [0x33u8; 16], attributes: vec![(radius::MESSAGE_AUTHENTICATOR, vec![0; 16])] }.encode();
    let mut mac = Hmac::<Md5>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(&status);
    status[22..38].copy_from_slice(&mac.finalize().into_bytes());
    let reply = udp_exchange(&status, "127.0.0.1:18120", WAIT).await.expect("no answer to Status-Server on the authentication port");
    assert_eq!(RadiusPacket::parse(&reply).unwrap().code, radius::ACCESS_ACCEPT);
    let reply = udp_exchange(&status, "127.0.0.1:18130", WAIT).await.expect("no answer to Status-Server on the accounting port");
    assert_eq!(RadiusPacket::parse(&reply).unwrap().code, radius::ACCOUNTING_RESPONSE);

    // Accounting-Request Start gets an Accounting-Response.
    let acct = RadiusPacket::create_accounting_request(12, vec![(40, vec![0, 0, 0, 1]), (44, b"session-1".to_vec())], SECRET.as_bytes());
    let acct_authenticator: [u8; 16] = acct[4..20].try_into().unwrap();
    let reply = udp_exchange(&acct, "127.0.0.1:18130", WAIT).await.expect("no Accounting-Response within timeout");
    let res = RadiusPacket::parse(&reply).unwrap();
    assert_eq!(res.code, radius::ACCOUNTING_RESPONSE);
    assert_eq!(res.identifier, 12);
    assert_response_authenticator(&reply, &acct_authenticator);
}

#[tokio::test]
async fn test_full_tacacs_flow() {
    let (_dir, config) = test_config();
    let engine = load_engine(&config).await;
    let tacacs = config.tacacs.unwrap();
    let server = TacacsServer::new(tacacs.addr, tacacs.clients, engine, tacacs.max_body_len);
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Retry until the spawned server is listening.
    let mut stream = timeout(WAIT, async {
        loop {
            match TcpStream::connect("127.0.0.1:4900").await {
                Ok(stream) => return stream,
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("TACACS+ server not listening within timeout");

    let nemo = tacacs_authenticate(&mut stream, 100, b"nemo").await;
    assert_eq!(nemo.seq_num, 2);
    assert_eq!(nemo.session_id, 100);
    assert_eq!(nemo.body[0], tacacs::AUTHEN_STATUS_PASS);

    let other = tacacs_authenticate(&mut stream, 101, b"mallory").await;
    assert_eq!(other.session_id, 101);
    assert_eq!(other.body[0], tacacs::AUTHEN_STATUS_FAIL);
}

/// Sends an encrypted Authentication START for `user` and returns the decrypted reply.
async fn tacacs_authenticate(stream: &mut TcpStream, session_id: u32, user: &[u8]) -> TacacsPacket {
    // START: action LOGIN, priv_lvl 1, authen_type PAP, service LOGIN, user only.
    let version = 0xc0;
    let mut body = vec![0x01, 0x01, 0x02, 0x01, user.len() as u8, 0, 0, 0];
    body.extend_from_slice(user);
    let request = TacacsPacket {
        version,
        type_: tacacs::TYPE_AUTHEN,
        seq_num: 1,
        flags: 0,
        session_id,
        body: TacacsPacket::encrypt_decrypt(&body, SECRET.as_bytes(), session_id, version, 1),
    };
    stream.write_all(&request.encode()).await.unwrap();

    let mut reply = timeout(WAIT, tacacs::read_packet(stream, tacacs::MAX_BODY_LEN))
        .await
        .expect("no TACACS+ reply within timeout")
        .unwrap()
        .expect("connection closed without reply");
    reply.body = TacacsPacket::encrypt_decrypt(&reply.body, SECRET.as_bytes(), reply.session_id, reply.version, reply.seq_num);
    reply
}

/// `all_accept`: an SQL handler checks the password, then a Lua handler only
/// allows one Calling-Station-Id.
#[tokio::test]
async fn test_auth_chain_sql_and_site_rule() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        dir.path().join("site.lua"),
        "if get_attr('Calling-Station-Id') == '02-00-00-00-00-01' then return 'accept' end return 'reject'\n",
    )
    .unwrap();
    let body = format!(
        "dictionary = \"{dictionary}\"\n[lua]\nscript = \"{script}\"\n\
         [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\nlisten = \"127.0.0.1\"\n\
         [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{raft}\"\napi = \"127.0.0.1:{api}\"\n\
         [auth]\npolicy = \"all_accept\"\n\
         [[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n\
         [[auth.handlers]]\ntype = \"lua\"\nscript = \"site.lua\"\n\
         [radius]\nbind = \"127.0.0.1\"\nauth_port = 18320\nacct_port = 18330\n\
         [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n",
        dictionary = root.join("../share/dictionary").display(),
        script = root.join("tests/fixtures/auth.lua").display(),
        raft = free_port(),
        api = free_port(),
    );
    let path = dir.path().join("rradius.toml");
    std::fs::write(&path, body).unwrap();
    let config = config::load(&path).unwrap();

    let db = Db::start(&config.cluster).await.unwrap();
    db.wait_healthy(Duration::from_secs(60)).await.unwrap();
    db.execute("CREATE TABLE users (username TEXT, password TEXT)").await.unwrap();
    db.execute("INSERT INTO users VALUES ('nemo', '{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=')").await.unwrap();
    let vms = Arc::new(LuaVms::new(256 << 20).unwrap());
    let chains = Arc::new(AuthChains::from_settings(config.auth.as_ref().unwrap(), &db, &vms, &LuaLimits::from_settings(&config.lua)).unwrap());
    let script = std::fs::read_to_string(&config.lua.script).unwrap();
    let engine = Arc::new(LuaAuthEngine::new(vms, db, script).unwrap());
    let radius = config.radius.as_ref().unwrap();
    let server = Arc::new(
        RadiusServer::new(radius.auth_addr, radius.acct_addr, radius.clients.clone(), engine, load_dictionary(&config)).with_auth_chains(chains),
    );
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // User-Password for "password" (one block): plaintext XOR MD5(secret + Request Authenticator).
    let authenticator = [0x42u8; 16];
    let mut block = [0u8; 16];
    block[..8].copy_from_slice(b"password");
    let mut hasher = Md5::new();
    hasher.update(SECRET.as_bytes());
    hasher.update(authenticator);
    let cipher: Vec<u8> = block.iter().zip(hasher.finalize()).map(|(p, k)| p ^ k).collect();

    let access = |id: u8, station: &str| {
        RadiusPacket::create_access_request(
            id,
            authenticator,
            vec![(1, b"nemo".to_vec()), (2, cipher.clone()), (31, station.as_bytes().to_vec())],
            SECRET.as_bytes(),
        )
    };
    let allowed = udp_exchange(&access(20, "02-00-00-00-00-01"), "127.0.0.1:18320", WAIT).await.expect("no reply");
    assert_eq!(RadiusPacket::parse(&allowed).unwrap().code, radius::ACCESS_ACCEPT);
    let blocked = udp_exchange(&access(21, "02-00-00-00-00-99"), "127.0.0.1:18320", WAIT).await.expect("no reply");
    assert_eq!(RadiusPacket::parse(&blocked).unwrap().code, radius::ACCESS_REJECT);
}
