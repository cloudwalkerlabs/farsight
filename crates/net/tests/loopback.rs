//! A server and a client over loopback: handshake, control stream, a large
//! frame through datagrams, and pings that overtake a keyframe.

use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use farsight_net::packetize::{EncodedFrame, Packetizer, Reassembler};
use farsight_net::sched::{Priority, Scheduler};
use farsight_net::{endpoint, stream};
use farsight_proto::codec::{Chroma, Codec, Encoding, Format, Mode};

const H264: Format = Format { codec: Codec::H264, chroma: Chroma::Yuv420, bit_depth: 8 };
use farsight_proto::control::{ClientMessage, Hello, ServerMessage, Welcome};
use farsight_proto::datagram::{Datagram, Ping, Pong};
use farsight_proto::layout::Layout;

const LAYOUT: Layout = Layout { width_px: 1280, height_px: 720, scale_120: 120, refresh_mhz: 60_000 };

#[tokio::test]
async fn frame_and_control_over_loopback() {
    let id = endpoint::Identity::generate().unwrap();
    let fingerprint = id.fingerprint();
    let server = endpoint::server(None, 0, &id).unwrap();
    let port = server.local_addr().unwrap().port();

    let frame_data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    let sent = frame_data.clone();
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
        assert_eq!(hello.layout, LAYOUT);
        stream::send(&mut send, &ServerMessage::Welcome(Welcome { encodings: vec![Encoding::Video(H264)] }))
            .await
            .unwrap();
        let max = conn.max_datagram_size().unwrap();
        let frame = EncodedFrame { data: &sent, keyframe: true, epoch: 1, capture_us: 1, encode_us: 1 };
        let sched = Scheduler::spawn(conn.clone(), 1_000_000_000, endpoint::DATAGRAM_BUFFER);
        sched.send_frame(Packetizer::new().packetize(&frame, max), true);
        // Hold the connection open until the client is done.
        conn.closed().await;
    });

    let client = endpoint::client(false).unwrap();
    let (conn, fp) = endpoint::connect(&client, SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await.unwrap();
    assert_eq!(fp.as_deref(), Some(fingerprint.as_str()));
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    stream::send(&mut send, &ClientMessage::Hello(Hello { decoders: vec![], layout: LAYOUT, mode: Mode::Text, audio: None, auth: farsight_proto::control::ClientAuth { key: [0; 32], signature: vec![] } })).await.unwrap();
    let Some(ServerMessage::Welcome(w)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
    assert_eq!(w.encodings, [Encoding::Video(H264)]);

    let mut rx = Reassembler::new(1_000_000);
    let frame = loop {
        let d = conn.read_datagram().await.unwrap();
        let Some(Datagram::Video(h, payload)) = Datagram::decode(&d) else { panic!() };
        if let Some(frame) = rx.push(h, payload, 0) {
            break frame;
        }
    };
    assert_eq!(frame.data, frame_data);
    conn.close(0u32.into(), b"done");
    server_task.await.unwrap();
}

/// A 1 MB keyframe paced at 20 Mbps takes 400 ms to send. Pings sent
/// meanwhile must come back at once, not after the frame.
#[tokio::test]
async fn pings_overtake_a_keyframe() {
    let id = endpoint::Identity::generate().unwrap();
    let server = endpoint::server(None, 0, &id).unwrap();
    let port = server.local_addr().unwrap().port();

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let sched = Scheduler::spawn(conn.clone(), 20_000_000, endpoint::DATAGRAM_BUFFER);
        let data = vec![0x55; 1_000_000];
        let frame = EncodedFrame { data: &data, keyframe: true, epoch: 1, capture_us: 0, encode_us: 0 };
        sched.send_frame(Packetizer::new().packetize(&frame, conn.max_datagram_size().unwrap()), true);
        while let Ok(d) = conn.read_datagram().await {
            if let Some(Datagram::Ping(p)) = Datagram::decode(&d) {
                sched.send(Priority::Input, Datagram::Pong(Pong { client_us: p.client_us, server_us: 0 }).to_vec().into());
            }
        }
    });

    let client = endpoint::client(false).unwrap();
    let (conn, _) = endpoint::connect(&client, SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await.unwrap();
    let start = Instant::now();
    let now_us = || start.elapsed().as_micros() as u64;
    let mut rx = Reassembler::new(10_000_000);
    let mut rtts = Vec::new();
    let mut next_ping = Duration::ZERO;
    let frame_at = loop {
        if start.elapsed() >= next_ping {
            conn.send_datagram(Datagram::Ping(Ping { client_us: now_us() }).to_vec().into()).unwrap();
            next_ping += Duration::from_millis(20);
        }
        let Ok(Ok(d)) = tokio::time::timeout(Duration::from_millis(5), conn.read_datagram()).await else { continue };
        match Datagram::decode(&d) {
            Some(Datagram::Pong(p)) => rtts.push(now_us() - p.client_us),
            Some(Datagram::Video(h, payload)) if rx.push(h, payload, 0).is_some() => break start.elapsed(),
            _ => {}
        }
    };
    assert!(frame_at >= Duration::from_millis(300), "the frame was not paced: {frame_at:?}");
    assert!(rtts.len() >= 10, "{rtts:?}");
    let worst = *rtts.iter().max().unwrap();
    assert!(worst < 20_000, "a ping waited {worst} µs behind video: {rtts:?}");
    conn.close(0u32.into(), b"done");
    server_task.abort();
}

/// Plaintext mode: a whole connection with the null crypto layer, the
/// client key's proof across it, and a TLS client turned away at once.
#[tokio::test]
async fn plaintext_over_loopback() {
    use farsight_net::auth::{ClientKey, verify};
    let server = endpoint::server_plain(Ipv6Addr::LOCALHOST.into(), 0).unwrap();
    let addr = SocketAddr::from((Ipv6Addr::LOCALHOST, server.local_addr().unwrap().port()));
    let dir = std::env::temp_dir().join(format!("farsight-plain-{}", std::process::id()));
    let key = ClientKey::load_or_generate(&dir.join("client_key")).unwrap();
    let public = key.public();

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
        assert_eq!(hello.auth.key, public);
        assert!(verify(&conn, &hello.auth.key, &hello.auth.signature));
        let mut forged = hello.auth.signature.clone();
        forged[0] ^= 1;
        assert!(!verify(&conn, &hello.auth.key, &forged));
        stream::send(&mut send, &ServerMessage::Welcome(Welcome { encodings: vec![] })).await.unwrap();
        conn.send_datagram(bytes::Bytes::from_static(b"plain datagram")).unwrap();
        conn.closed().await;
    });

    let client = endpoint::client(true).unwrap();
    let (conn, fp) = endpoint::connect(&client, addr).await.unwrap();
    assert_eq!(fp, None);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let auth = farsight_proto::control::ClientAuth { key: key.public(), signature: key.sign(&conn).unwrap() };
    let hello = Hello { decoders: vec![], layout: LAYOUT, mode: Mode::Text, audio: None, auth };
    stream::send(&mut send, &ClientMessage::Hello(hello)).await.unwrap();
    let Some(ServerMessage::Welcome(_)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
    assert_eq!(&conn.read_datagram().await.unwrap()[..], b"plain datagram");
    conn.close(0u32.into(), b"done");
    server_task.await.unwrap();

    // TLS against a plaintext server: version negotiation, not a timeout.
    let server = endpoint::server_plain(Ipv6Addr::LOCALHOST.into(), 0).unwrap();
    let addr = SocketAddr::from((Ipv6Addr::LOCALHOST, server.local_addr().unwrap().port()));
    let started = Instant::now();
    assert!(endpoint::connect(&endpoint::client(false).unwrap(), addr).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&dir);
}
