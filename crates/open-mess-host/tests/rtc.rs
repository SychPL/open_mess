//! Module media connections (`rtc`) against a second WebRTC host on loopback that answers with an
//! ECDSA P-384 certificate signing with SHA-256, as Teams' media processor does (spec
//! 2026-10-29-live-captions, 5.2). The test-echo module logs what its `events` export receives; the
//! tests read those logs.

// clippy.toml allows unwrap in #[test] functions only; the peer and log helpers are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use common::load;
use open_mess_host::{Engine, ModuleHandle, limits};
use p384::ecdsa::signature::hazmat::PrehashSigner;
use rtc::crypto::{CryptoError, PublicKey, PublicKeyEncoding, SignatureScheme, SigningKey};
use rtc::peer_connection::certificate::RTCCertificate;
use rustls::pki_types::CertificateDer;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCSessionDescription, SettingEngineBuilder,
};

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static INIT: Once = Once::new();
    INIT.call_once(|| tracing_subscriber::fmt().with_writer(|| Capture).with_max_level(tracing::Level::DEBUG).init());
}

fn logs() -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned()
}

async fn wait_for_log(needle: &str) -> bool {
    for _ in 0..200 {
        if logs().contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn echo() -> ModuleHandle {
    capture_logs();
    load(&Engine::with_limits(limits::for_tests()).unwrap(), "open_mess_test_echo").await
}

async fn call(module: &ModuleHandle, tool: &str, args: Value) -> Value {
    serde_json::from_str(&module.call("a", tool, &args.to_string()).await.unwrap()).unwrap()
}

/// The far end's self-signed certificate: ECDSA P-384, signed with SHA-256, made once for these
/// tests (with Python's `cryptography`) together with its private scalar below - a test key, not
/// a secret.
const PEER_CERTIFICATE: &[&str] = &[
    "308201673081eea003020102020101300a06082a8648ce3d040302301e311c301a06035504030c136f70656e5f6d6573",
    "73207465737420706565723020170d3236303130313030303030305a180f32313235313230383030303030305a301e31",
    "1c301a06035504030c136f70656e5f6d657373207465737420706565723076301006072a8648ce3d020106052b810400",
    "2203620004bf9744877b97e40754757db63c2bb6ce1c6e458fc10546f2cf33d434b92d504b984e7f7100b9c827834207",
    "90b8351fcd2217a86292c4438edb3ed169e1a374e9c5a0bfc8a94b5c977f3518f0a8b05fb3de41985a04cc29a1f77b16",
    "ae5d991d0b300a06082a8648ce3d0403020368003065023100c226a1a0187a2adc497e8f605f5201a24c1336d6f74f76",
    "63ca61b18feb21e036ef5f977bb5718d476e2d2bcddc49953e02301bd353c459157c2b68b9c4c4e269739bf541bd2088",
    "e87a23ac6a6175dd8743ebe7dc8d8db9f4fdda5bccc2a50f3d34a5",
];
const PEER_SCALAR: &str = "d6cfb717f0d9c1cf24a52b06dbb84a78d6982e96df40f33b3b8e0046d3c31ae11c627cbcad6fb22954c29f6d73c08f44";

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// The far end's key: P-384, signing with SHA-256 and nothing else - the pairing ring cannot sign
/// but must verify. When it signs, the DTLS library names the TLS pair (ecdsa, sha256)
/// `EcdsaP256Sha256` whatever the curve; on the wire it is the pair Teams sends.
struct P384Sha256 {
    key: p384::ecdsa::SigningKey,
    public: Vec<u8>,
}

impl SigningKey for P384Sha256 {
    fn supports(&self, scheme: SignatureScheme) -> bool {
        matches!(scheme, SignatureScheme::EcdsaP256Sha256 | SignatureScheme::EcdsaP384Sha256)
    }

    fn public_key(&self) -> PublicKey<'_> {
        PublicKey { encoding: PublicKeyEncoding::EcUncompressedPoint, bytes: &self.public }
    }

    fn sign(&self, _scheme: SignatureScheme, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let signature: p384::ecdsa::Signature = self.key.sign_prehash(&sha2::Sha256::digest(message)).expect("a signature");
        Ok(signature.to_der().as_bytes().to_vec())
    }
}

fn peer_certificate() -> RTCCertificate {
    let key = p384::ecdsa::SigningKey::from_slice(&hex(PEER_SCALAR)).unwrap();
    let public = key.verifying_key().to_sec1_point(false).as_bytes().to_vec();
    let der = CertificateDer::from(hex(&PEER_CERTIFICATE.concat()));
    RTCCertificate::from_signing_key(vec![der], Arc::new(P384Sha256 { key, public }), std::time::SystemTime::now() + Duration::from_secs(3600))
}

/// The far end: what the test peer received and whether it ever got a media track.
struct PeerEvents {
    channels: mpsc::Sender<Arc<dyn DataChannel>>,
    gathered: mpsc::Sender<()>,
    track: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerEvents {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.try_send(()).ok();
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        self.channels.try_send(channel).ok();
    }

    async fn on_track(&self, _track: Arc<dyn TrackRemote>) {
        self.track.store(true, Ordering::SeqCst);
    }
}

struct Peer {
    pc: Arc<dyn PeerConnection>,
    channels: mpsc::Receiver<Arc<dyn DataChannel>>,
    track: Arc<AtomicBool>,
}

/// A WebRTC host on loopback that answers `offer` with a P-384 certificate signing with SHA-256.
async fn answer(offer: &str) -> (Peer, String) {
    let certificate = peer_certificate();
    let (channels_tx, channels) = mpsc::channel(4);
    let (gathered_tx, mut gathered) = mpsc::channel(1);
    let track = Arc::new(AtomicBool::new(false));
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().with_certificates(vec![certificate]).build())
        .with_media_engine(MediaEngine::default())
        .with_setting_engine(SettingEngineBuilder::new().with_include_loopback_candidate(true).build())
        .with_handler(Arc::new(PeerEvents { channels: channels_tx, gathered: gathered_tx, track: track.clone() }))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);
    pc.set_remote_description(RTCSessionDescription::offer(offer.to_string()).unwrap()).await.unwrap();
    let answer = pc.create_answer(None).await.unwrap();
    pc.set_local_description(answer).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await.ok();
    let sdp = pc.local_description().await.unwrap().sdp;
    (Peer { pc, channels, track }, sdp)
}

async fn open(module: &ModuleHandle, label: &str) -> (u64, String) {
    let opened = call(module, "rtc-open", json!({"label": label})).await;
    (opened["conn"].as_u64().expect("a connection"), opened["offer"].as_str().unwrap().to_string())
}

fn sections(sdp: &str) -> Vec<&str> {
    sdp.lines().filter(|line| line.starts_with("m=")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_offer_has_only_a_data_channel() {
    let module = echo().await;
    let (conn, offer) = open(&module, "main-channel").await;
    let media = sections(&offer);
    assert_eq!(media.len(), 1, "{media:?}");
    assert!(media[0].starts_with("m=application ") && media[0].contains("webrtc-datachannel"), "{media:?}");
    assert!(offer.lines().any(|line| line.starts_with("a=candidate:") && line.contains("127.0.0.1")));
    call(&module, "rtc-close", json!({"conn": conn})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_from_a_p384_peer_reach_the_module_in_batches() {
    let module = echo().await;
    let (conn, offer) = open(&module, "main-channel").await;
    let (mut peer, answer) = answer(&offer).await;
    assert_eq!(call(&module, "rtc-accept", json!({"conn": conn, "answer": answer})).await, json!({}));
    let channel = tokio::time::timeout(Duration::from_secs(10), peer.channels.recv()).await.unwrap().unwrap();
    assert_eq!(channel.label().await.unwrap(), "main-channel");
    while !matches!(tokio::time::timeout(Duration::from_secs(10), channel.poll()).await.unwrap(), Some(DataChannelEvent::OnOpen)) {}
    for n in 0..3 {
        channel.send_text(&format!("caption-{conn}-{n}")).await.unwrap();
    }
    assert!(wait_for_log(&format!("rtc {conn} state connected")).await, "{}", logs());
    assert!(wait_for_log(&format!("caption-{conn}-2")).await, "{}", logs());
    // Nothing but the data channel: the far end never got a media track.
    assert!(!peer.track.load(Ordering::SeqCst));
    // The module's close ends the connection on the far end too.
    call(&module, "rtc-close", json!({"conn": conn})).await;
    peer.pc.close().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_with_audio_or_without_an_allowed_candidate_is_refused() {
    let module = echo().await;
    let (conn, offer) = open(&module, "main-channel").await;
    let (peer, answer) = answer(&offer).await;
    // An audio section in which the far end would send sound.
    let audio = format!("{answer}m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\na=sendrecv\r\na=rtpmap:111 opus/48000/2\r\n");
    let refused = call(&module, "rtc-accept", json!({"conn": conn, "answer": audio})).await;
    assert_eq!(refused["error"], "blocked", "{refused}");
    // Every candidate outside the module's ranges (52.112.0.0/14) and not loopback.
    let elsewhere = answer.replace("127.0.0.1", "8.8.8.8");
    let refused = call(&module, "rtc-accept", json!({"conn": conn, "answer": elsewhere})).await;
    assert_eq!(refused["error"], "blocked", "{refused}");
    assert!(refused["why"].as_str().unwrap().contains("media ranges"), "{refused}");
    peer.pc.close().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fifth_connection_is_refused() {
    let module = echo().await;
    let mut conns = Vec::new();
    for _ in 0..limits::MAX_RTC {
        conns.push(open(&module, "main-channel").await.0);
    }
    let refused = call(&module, "rtc-open", json!({"label": "main-channel"})).await;
    assert_eq!(refused["error"], "blocked", "{refused}");
    // A closed one makes room again.
    call(&module, "rtc-close", json!({"conn": conns[0]})).await;
    assert!(open(&module, "main-channel").await.0 > 0);
}
