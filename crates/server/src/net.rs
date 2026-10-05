//! The server's network side: a tokio thread that owns the QUIC endpoint
//! and talks to the host's event loop through two channels.
//!
//! One client is served at a time. A new connection takes over from the
//! current one, which is closed; the session itself carries on (§6).
//! Messages are tagged with the connection they belong to, so nothing meant
//! for an old connection reaches a new one.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::{EncodedFrame, Packetizer};
use farsight_net::quinn::{self, Connection};
use farsight_net::sched::{Priority, Scheduler};
use farsight_net::{endpoint, stream};
use farsight_proto::control::{ClientMessage, Hello, ServerMessage};
use farsight_proto::datagram::{self, Datagram, Pong};
use farsight_proto::tiles::TilesHeader;
use farsight_proto::input::InputPacket;
use smithay::reexports::calloop::channel::Sender as HostSender;
use tokio::sync::mpsc;

/// Identifies one client connection.
pub type ConnId = u64;

/// From the network to the host.
#[derive(Debug)]
pub enum ToHost {
    /// A client said hello. The host answers with a `Welcome` and a
    /// keyframe.
    Connected(ConnId, Hello),
    Message(ConnId, ClientMessage),
    Input(ConnId, InputPacket),
    Disconnected(ConnId),
}

/// From the host (or the encode thread) to the network.
#[derive(Debug)]
pub enum ToNet {
    Frame(ConnId, Frame),
    Tiles(ConnId, Tiles),
    Message(ConnId, ServerMessage),
    /// The session is ending: close every connection with this reason.
    Shutdown(String),
}

#[derive(Debug)]
pub struct Frame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub epoch: u16,
    pub capture_us: u64,
    pub encode_us: u32,
}

/// One tiles update, ready to go.
#[derive(Debug)]
pub struct Tiles {
    pub update: crate::encode::tiles::Update,
    pub epoch: u16,
    pub capture_us: u64,
    pub encode_us: u32,
}

pub struct Options {
    pub port: u16,
    pub identity: endpoint::Identity,
    /// Video is paced at this rate until there is a congestion controller
    /// (M4).
    pub rate_bps: u64,
    /// The host's clock, which pongs and frame timestamps are in.
    pub start: Instant,
}

/// The current connection, if any.
struct Current {
    id: ConnId,
    conn: Connection,
    sched: Scheduler,
    control: mpsc::UnboundedSender<ServerMessage>,
}

/// Starts the network thread and returns where to send it frames and
/// messages.
pub fn spawn(opts: Options, host: HostSender<ToHost>) -> anyhow::Result<mpsc::UnboundedSender<ToNet>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("farsight-net")
        .enable_all()
        .build()?;
    let endpoint = {
        let _guard = rt.enter();
        endpoint::server(opts.port, &opts.identity)?
    };
    tracing::info!(
        addr = %endpoint.local_addr()?, fingerprint = opts.identity.fingerprint(),
        rate_mbps = opts.rate_bps / 1_000_000, "listening"
    );
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new().name("farsight-net".into()).spawn(move || {
        rt.block_on(run(endpoint, opts, host, rx));
    })?;
    Ok(tx)
}

async fn run(endpoint: quinn::Endpoint, opts: Options, host: HostSender<ToHost>, rx: mpsc::UnboundedReceiver<ToNet>) {
    let current: Arc<Mutex<Option<Current>>> = Arc::default();
    tokio::spawn(dispatch(rx, current.clone()));
    let mut next_id: ConnId = 0;
    while let Some(incoming) = endpoint.accept().await {
        next_id += 1;
        let (id, host, current, rate_bps, start) = (next_id, host.clone(), current.clone(), opts.rate_bps, opts.start);
        tokio::spawn(async move {
            let addr = incoming.remote_address();
            match serve(id, incoming, host.clone(), current.clone(), rate_bps, start).await {
                Ok(()) => tracing::info!(id, %addr, "client gone"),
                Err(err) => tracing::info!(id, %addr, "client gone: {err:#}"),
            }
            let mut cur = current.lock().unwrap();
            if cur.as_ref().is_some_and(|c| c.id == id) {
                *cur = None;
            }
            let _ = host.send(ToHost::Disconnected(id));
        });
    }
}

/// Routes the host's output to the current connection.
async fn dispatch(mut rx: mpsc::UnboundedReceiver<ToNet>, current: Arc<Mutex<Option<Current>>>) {
    let mut packetizer = Packetizer::new();
    let mut next_update: u32 = 0;
    while let Some(msg) = rx.recv().await {
        let cur = current.lock().unwrap();
        let Some(c) = cur.as_ref() else { continue };
        match msg {
            ToNet::Shutdown(reason) => {
                c.conn.close(2u32.into(), reason.as_bytes());
            }
            ToNet::Frame(id, f) if id == c.id => {
                let Some(max) = c.conn.max_datagram_size() else { continue };
                let frame = EncodedFrame {
                    data: &f.data,
                    keyframe: f.keyframe,
                    epoch: f.epoch,
                    capture_us: f.capture_us,
                    encode_us: f.encode_us,
                };
                c.sched.send_frame(packetizer.packetize(&frame, max), f.keyframe);
            }
            ToNet::Tiles(id, t) if id == c.id => {
                let count = t.update.bodies.len();
                if count == 0 {
                    continue;
                }
                let mut header = TilesHeader {
                    epoch: t.epoch,
                    update: next_update,
                    index: 0,
                    count: count.min(u16::MAX as usize) as u16,
                    capture_us: t.capture_us,
                    encode_us: t.encode_us,
                    bounds: t.update.bounds,
                };
                next_update = next_update.wrapping_add(1);
                let datagrams = t
                    .update
                    .bodies
                    .iter()
                    .take(u16::MAX as usize)
                    .enumerate()
                    .map(|(i, body)| {
                        header.index = i as u16;
                        let mut out = Vec::new();
                        datagram::encode_tiles(&header, body, &mut out);
                        Bytes::from(out)
                    })
                    .collect();
                // Never superseded: each update carries only what changed.
                c.sched.send_frame(datagrams, false);
            }
            ToNet::Message(id, m) if id == c.id => {
                let _ = c.control.send(m);
            }
            _ => {} // for a connection that has gone
        }
    }
}

async fn serve(
    id: ConnId,
    incoming: quinn::Incoming,
    host: HostSender<ToHost>,
    current: Arc<Mutex<Option<Current>>>,
    rate_bps: u64,
    start: Instant,
) -> anyhow::Result<()> {
    let conn = incoming.await.context("handshake")?;
    tracing::info!(id, addr = %conn.remote_address(), "client connected");
    let (mut send, mut recv) = conn.accept_bi().await.context("waiting for the control stream")?;
    let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await? else {
        anyhow::bail!("the client did not start with Hello");
    };
    tracing::info!(id, layout = ?hello.layout, decoders = hello.decoders.len(), "hello");

    let sched = Scheduler::spawn(conn.clone(), rate_bps, endpoint::DATAGRAM_BUFFER);
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let next = Current { id, conn: conn.clone(), sched: sched.clone(), control: control_tx };
    let previous = current.lock().unwrap().replace(next);
    if let Some(prev) = previous {
        tracing::info!(id, previous = prev.id, "taking over the session");
        prev.conn.close(1u32.into(), b"another client took over");
    }
    let _ = host.send(ToHost::Connected(id, hello));

    let writer = async {
        while let Some(msg) = control_rx.recv().await {
            stream::send(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    };
    let reader = async {
        while let Some(msg) = stream::recv::<ClientMessage>(&mut recv).await? {
            let _ = host.send(ToHost::Message(id, msg));
        }
        anyhow::Ok(())
    };
    let datagrams = async {
        let err = loop {
            let d = match conn.read_datagram().await {
                Ok(d) => d,
                Err(err) => break err,
            };
            match Datagram::decode(&d) {
                Some(Datagram::Input(p)) => {
                    let _ = host.send(ToHost::Input(id, p));
                }
                Some(Datagram::Ping(p)) => {
                    let pong = Pong { client_us: p.client_us, server_us: start.elapsed().as_micros() as u64 };
                    sched.send(Priority::Input, Bytes::from(Datagram::Pong(pong).to_vec()));
                }
                _ => {}
            }
        };
        Err::<(), _>(anyhow::Error::from(err))
    };
    tokio::select! {
        r = writer => r,
        r = reader => r,
        r = datagrams => r,
    }
}
