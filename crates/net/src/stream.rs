//! Length-prefixed control messages on a QUIC stream
//! (`farsight_proto::control`).

use anyhow::Context;
use farsight_proto::control;
use quinn::{ReadExactError, RecvStream, SendStream};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub async fn send<T: Serialize>(stream: &mut SendStream, msg: &T) -> anyhow::Result<()> {
    stream.write_all(&control::encode_framed(msg)).await.context("writing control message")
}

/// The next message, or `None` once the peer has finished the stream.
pub async fn recv<T: DeserializeOwned>(stream: &mut RecvStream) -> anyhow::Result<Option<T>> {
    let mut prefix = [0; 4];
    match stream.read_exact(&mut prefix).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e).context("reading control message"),
    }
    let mut body = vec![0; control::frame_len(prefix)?];
    stream.read_exact(&mut body).await.context("reading control message")?;
    Ok(Some(control::decode(&body)?))
}
