//! The session's microphone (docs/design.md §8): a PipeWire stream that is
//! an `Audio/Source` node, `farsight-mic`, which WirePlumber makes the
//! session's default source. It only runs while an app records from it;
//! then the controlling client is asked for its microphone (`MicDemand`),
//! and what it sends plays into the node through a jitter buffer, as the
//! session's audio does on the client.
//!
//! The datagrams' capture times are on the server's clock (the client
//! converts them), so the buffer's latency is capture to source.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use farsight_audio::{AudioStats, Player};
use farsight_proto::audio::{AudioPacket, MIC};

#[derive(Default)]
pub struct Mic {
    /// An app records: the client sends, and this plays it.
    demand: AtomicBool,
    player: Mutex<Option<Player>>,
}

impl Mic {
    pub fn demand(&self) -> bool {
        self.demand.load(Ordering::Relaxed)
    }

    /// An app started or stopped recording; true if that changed
    /// anything. Each recording starts with an empty buffer.
    pub fn set_demand(&self, on: bool) -> bool {
        if self.demand.swap(on, Ordering::Relaxed) == on {
            return false;
        }
        let player = match on {
            true => Player::new(MIC).map_err(|err| tracing::warn!("microphone: {err:#}")).ok(),
            false => None,
        };
        *self.player.lock().unwrap() = player;
        true
    }

    /// A datagram from the client, at `now_us` on the server's clock.
    pub fn push(&self, p: &AudioPacket, now_us: u64) {
        if let Some(player) = self.player.lock().unwrap().as_mut() {
            player.push(p, now_us);
        }
    }

    /// One graph cycle's samples, mono, to be heard at `now_us`.
    pub fn fill(&self, out: &mut [f32], now_us: u64) {
        match self.player.lock().unwrap().as_mut() {
            Some(player) => player.pull(out, now_us, 0, Some(0)),
            None => out.fill(0.0),
        }
    }

    /// Statistics since the last call, while an app records.
    pub fn take_stats(&self) -> Option<AudioStats> {
        self.player.lock().unwrap().as_mut().map(Player::take_stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_resets_the_buffer() {
        let mic = Mic::default();
        let mut out = [1.0; 240];
        mic.fill(&mut out, 0);
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(mic.set_demand(true));
        assert!(!mic.set_demand(true));
        assert!(mic.take_stats().is_some());
        assert!(mic.set_demand(false));
        assert!(mic.take_stats().is_none());
    }
}
