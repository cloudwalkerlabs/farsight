//! A server and a client over loopback: handshake, control stream, and a
//! large frame through datagrams.

use std::net::{Ipv6Addr, SocketAddr};

use farsight_net::packetize::{EncodedFrame, Packetizer, Reassembler};
use farsight_net::{endpoint, stream};
use farsight_proto::codec::Codec;
use farsight_proto::control::{ClientMessage, Hello, ServerMessage, Welcome};
use farsight_proto::datagram::Datagram;
use farsight_proto::layout::Layout;

const LAYOUT: Layout = Layout { width_px: 1280, height_px: 720, scale_120: 120, refresh_mhz: 60_000 };

#[tokio::test]
async fn frame_and_control_over_loopback() {
    let id = endpoint::Identity::generate().unwrap();
    let fingerprint = id.fingerprint();
    let server = endpoint::server(0, &id).unwrap();
    let port = server.local_addr().unwrap().port();

    let frame_data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    let sent = frame_data.clone();
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
        assert_eq!(hello.layout, LAYOUT);
        stream::send(&mut send, &ServerMessage::Welcome(Welcome { codec: Codec::H264, layout: hello.layout }))
            .await
            .unwrap();
        let max = conn.max_datagram_size().unwrap();
        let frame = EncodedFrame { data: &sent, keyframe: true, epoch: 1, capture_us: 1, encode_us: 1 };
        for d in Packetizer::new().packetize(&frame, max) {
            conn.send_datagram(d).unwrap();
        }
        // Hold the connection open until the client is done.
        conn.closed().await;
    });

    let client = endpoint::client().unwrap();
    let (conn, fp) = endpoint::connect(&client, SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await.unwrap();
    assert_eq!(fp, fingerprint);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    stream::send(&mut send, &ClientMessage::Hello(Hello { decoders: vec![], layout: LAYOUT })).await.unwrap();
    let Some(ServerMessage::Welcome(w)) = stream::recv(&mut recv).await.unwrap() else { panic!() };
    assert_eq!(w.codec, Codec::H264);

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
