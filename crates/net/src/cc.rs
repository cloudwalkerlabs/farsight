//! Congestion control for media (`docs/design.md` §1).
//!
//! quinn's controllers are built for bulk transfer: they halve their window
//! on every loss, so a link with a few percent of random loss, which FEC
//! shrugs off, would starve the stream. farsight's has two halves:
//!
//! - [`Media`], quinn's `Controller`, keeps a window of about twice what the
//!   estimated rate puts in flight in a round trip, whatever is lost, and
//!   records what quinn tells it: bytes sent and acked, and the shortest
//!   round trip of each batch of acks.
//! - [`RateControl`] turns those records into a rate, every
//!   [`crate::path::INTERVAL`]. It is delay-based, after SCReAM and GCC:
//!   the queue the stream builds on its path is the shortest recent round
//!   trip minus the shortest of the last [`BASE_WINDOW`]. A queue past
//!   [`QUEUE_HIGH`] cuts the rate towards what is being delivered; a queue
//!   under [`QUEUE_LOW`] lets it grow, as long as the stream is using it.
//!   Loss counts only when it is heavy ([`LOSS_HIGH`]): FEC takes care of
//!   the rest.
//!
//! The rate paces video ([`crate::sched`]) and, less a margin for FEC and
//! audio, is the encoder's target.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::Connection;
use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;

/// The least the rate goes down to.
pub const MIN_RATE: u64 = 500_000;

/// Where the rate starts, unless the most allowed is less.
pub const START_RATE: u64 = 20_000_000;

/// How long the base round trip is remembered: a route change that makes
/// it longer is believed after this long.
pub const BASE_WINDOW: Duration = Duration::from_secs(10);

/// Queueing delay that is congestion, and delay that is none.
pub const QUEUE_HIGH: Duration = Duration::from_millis(10);
pub const QUEUE_LOW: Duration = Duration::from_millis(4);

/// Loss this heavy is congestion, not noise.
pub const LOSS_HIGH: f64 = 0.2;

/// Growth per interval: while starting, and after the first congestion.
const STARTUP_GROWTH: f64 = 1.5;
const GROWTH: f64 = 1.05;
/// Growth is at least this much per interval, so a low rate recovers.
const MIN_STEP: f64 = 50_000.0;

/// A cut takes the rate to this share of what is delivered, but never
/// below this share of what it was.
const BACKOFF: f64 = 0.9;
const MAX_CUT: f64 = 0.7;

/// After a cut, the rate holds for this many round trips (and at least
/// [`MIN_HOLD`]) so the queue can drain before it is judged again.
const HOLD_RTTS: u32 = 2;
const MIN_HOLD: Duration = Duration::from_millis(100);

/// The stream is using the rate when it sends at least this share of it;
/// below that it is limited by what there is to send, and the rate isn't
/// raised further.
const IN_USE: f64 = 0.5;

/// The window covers this much beyond the round trip, for acks the peer
/// delays.
const ACK_SLACK: Duration = Duration::from_millis(25);

/// The window is never less than this many full packets.
const MIN_WINDOW_PACKETS: u64 = 16;

/// What [`Media`] saw since the last [`Shared::take`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Sample {
    /// The shortest round trip among the packets acked: the one whose ack
    /// waited least at the peer.
    pub rtt: Option<Duration>,
    pub sent_bytes: u64,
    pub acked_bytes: u64,
}

/// Shared between a connection's [`Media`] and whoever runs its
/// [`RateControl`].
#[derive(Debug)]
pub struct Shared {
    rate_bps: AtomicU64,
    sample: Mutex<Sample>,
}

impl Shared {
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }

    pub fn set_rate_bps(&self, rate: u64) {
        self.rate_bps.store(rate, Ordering::Relaxed);
    }

    /// What was seen since the last call.
    pub fn take(&self) -> Sample {
        std::mem::take(&mut *self.sample.lock().unwrap())
    }
}

/// Builds a [`Media`] controller for each connection. Its window follows
/// the rate its [`RateControl`] sets, starting at [`START_RATE`].
#[derive(Debug)]
pub struct Factory;

impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, _now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let shared = Shared { rate_bps: AtomicU64::new(START_RATE), sample: Mutex::default() };
        Box::new(Media { shared: Arc::new(shared), mtu: current_mtu, srtt: Duration::from_millis(100) })
    }
}

/// quinn's congestion controller for a farsight connection.
#[derive(Debug, Clone)]
pub struct Media {
    shared: Arc<Shared>,
    mtu: u16,
    srtt: Duration,
}

/// The [`Shared`] state of `conn`'s controller, if it is a [`Media`].
pub fn shared(conn: &Connection) -> Option<Arc<Shared>> {
    conn.congestion_state().into_any().downcast::<Media>().ok().map(|m| m.shared)
}

impl Controller for Media {
    fn on_sent(&mut self, _now: Instant, bytes: u64, _last_packet_number: u64) {
        self.shared.sample.lock().unwrap().sent_bytes += bytes;
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.srtt = rtt.get();
        let sample_rtt = now.saturating_duration_since(sent);
        let mut s = self.shared.sample.lock().unwrap();
        s.acked_bytes += bytes;
        s.rtt = Some(s.rtt.map_or(sample_rtt, |r| r.min(sample_rtt)));
    }

    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        // Loss is FEC's business; the rate reacts to delay and heavy loss.
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
    }

    fn window(&self) -> u64 {
        let bytes_per_s = self.shared.rate_bps() as f64 / 8.0;
        let in_flight = bytes_per_s * (self.srtt + ACK_SLACK).as_secs_f64();
        (2.0 * in_flight) as u64 + MIN_WINDOW_PACKETS * self.mtu as u64
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The rate decision, with no clocks or sockets of its own.
#[derive(Debug)]
pub struct RateControl {
    rate: f64,
    max: f64,
    /// Each interval's shortest round trip, for the base.
    rtts: VecDeque<(Instant, Duration)>,
    startup: bool,
    hold_until: Option<Instant>,
}

/// What [`RateControl::update`] saw, for logs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Status {
    pub queue: Option<Duration>,
    pub base: Option<Duration>,
    pub sending_bps: f64,
    pub delivered_bps: f64,
}

impl RateControl {
    pub fn new(max_rate_bps: u64) -> Self {
        let max = max_rate_bps.max(MIN_RATE) as f64;
        Self { rate: (START_RATE as f64).min(max), max, rtts: VecDeque::new(), startup: true, hold_until: None }
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate as u64
    }

    /// Takes one interval's `sample`, `dt` long, and the loss rate, and
    /// returns the new rate.
    pub fn update(&mut self, now: Instant, dt: Duration, sample: Sample, loss: f64) -> (u64, Status) {
        let secs = dt.as_secs_f64().max(1e-3);
        let sending = sample.sent_bytes as f64 * 8.0 / secs;
        let delivered = sample.acked_bytes as f64 * 8.0 / secs;
        let mut status = Status { sending_bps: sending, delivered_bps: delivered, ..Status::default() };
        let Some(rtt) = sample.rtt else { return (self.rate_bps(), status) };
        while self.rtts.front().is_some_and(|&(t, _)| now.duration_since(t) > BASE_WINDOW) {
            self.rtts.pop_front();
        }
        self.rtts.push_back((now, rtt));
        let base = self.rtts.iter().map(|&(_, r)| r).min().unwrap_or(rtt);
        let queue = rtt.saturating_sub(base);
        (status.queue, status.base) = (Some(queue), Some(base));

        let holding = self.hold_until.is_some_and(|t| now < t);
        if (queue > QUEUE_HIGH || loss > LOSS_HIGH) && !holding {
            let mut rate = (self.rate.min(delivered) * BACKOFF).max(self.rate * MAX_CUT);
            if loss > LOSS_HIGH {
                rate = rate.min(self.rate * (1.0 - loss / 2.0));
            }
            tracing::debug!(
                from_mbps = self.rate / 1e6,
                to_mbps = rate / 1e6,
                queue_ms = queue.as_secs_f64() * 1e3,
                loss,
                delivered_mbps = delivered / 1e6,
                "rate cut"
            );
            self.rate = rate;
            self.startup = false;
            self.hold_until = Some(now + (base * HOLD_RTTS).max(MIN_HOLD));
        } else if queue < QUEUE_LOW && !holding && sending >= self.rate * IN_USE {
            let growth = if self.startup { STARTUP_GROWTH } else { GROWTH };
            self.rate = (self.rate * growth).max(self.rate + MIN_STEP);
        }
        self.rate = self.rate.clamp(MIN_RATE as f64, self.max);
        (self.rate_bps(), status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: Duration = Duration::from_millis(50);

    fn sample(rtt_ms: f64, sent_bps: f64, acked_bps: f64) -> Sample {
        let bytes = |bps: f64| (bps / 8.0 * DT.as_secs_f64()) as u64;
        Sample {
            rtt: Some(Duration::from_secs_f64(rtt_ms / 1000.0)),
            sent_bytes: bytes(sent_bps),
            acked_bytes: bytes(acked_bps),
        }
    }

    #[test]
    fn grows_while_used_and_holds_while_idle() {
        let mut t = Instant::now();
        let mut rc = RateControl::new(100_000_000);
        let start = rc.rate_bps() as f64;
        // Little to send: no reason to grow.
        for _ in 0..10 {
            t += DT;
            rc.update(t, DT, sample(1.0, 1e6, 1e6), 0.0);
        }
        assert_eq!(rc.rate_bps() as f64, start);
        // Using it: it grows, fast while starting.
        t += DT;
        let (r, _) = rc.update(t, DT, sample(1.0, start, start), 0.0);
        assert_eq!(r as f64, start * STARTUP_GROWTH);
        for _ in 0..20 {
            t += DT;
            let r = rc.rate_bps() as f64;
            rc.update(t, DT, sample(1.0, r, r), 0.0);
        }
        assert_eq!(rc.rate_bps(), 100_000_000, "capped at the most allowed");
    }

    #[test]
    fn a_queue_cuts_towards_delivery_then_holds() {
        let mut t = Instant::now();
        let mut rc = RateControl::new(100_000_000);
        for _ in 0..5 {
            t += DT;
            rc.update(t, DT, sample(5.0, 1e6, 1e6), 0.0);
        }
        // A 10 Mbit/s bottleneck: sending 20, delivering 10, and a queue.
        t += DT;
        let (r, s) = rc.update(t, DT, sample(30.0, 20e6, 10e6), 0.0);
        assert_eq!(s.queue, Some(Duration::from_millis(25)));
        assert_eq!(r, 14_000_000, "no more than 30% at once");
        t += DT;
        let (held, _) = rc.update(t, DT, sample(30.0, 14e6, 10e6), 0.0);
        assert_eq!(held, r, "holds while the queue drains");
        t += MIN_HOLD;
        let (r, _) = rc.update(t, DT, sample(30.0, 14e6, 10e6), 0.0);
        assert_eq!(r, 9_800_000);
        // Drained: it grows again, slowly now.
        t += MIN_HOLD;
        let (r2, _) = rc.update(t, DT, sample(5.0, 9.8e6, 9.8e6), 0.0);
        assert_eq!(r2 as f64, (r as f64 * GROWTH).round());
    }

    #[test]
    fn random_loss_is_ignored_and_heavy_loss_is_not() {
        let mut t = Instant::now();
        let mut rc = RateControl::new(100_000_000);
        let r0 = rc.rate_bps();
        t += DT;
        assert_eq!(rc.update(t, DT, sample(1.0, 1e6, 1e6), 0.1).0, r0);
        t += DT;
        let (r, _) = rc.update(t, DT, sample(1.0, 1e6, 1e6), 0.4);
        assert_eq!(r, (r0 as f64 * MAX_CUT) as u64);
    }

    #[test]
    fn the_base_forgets_old_round_trips() {
        let mut t = Instant::now();
        let mut rc = RateControl::new(100_000_000);
        rc.update(t, DT, sample(1.0, 0.0, 0.0), 0.0);
        t += BASE_WINDOW + DT;
        let (_, s) = rc.update(t, DT, sample(40.0, 0.0, 0.0), 0.0);
        assert_eq!(s.queue, Some(Duration::ZERO));
    }

    #[test]
    fn the_window_covers_a_round_trip_at_the_rate() {
        let media = Factory.build_media(1200);
        media.shared.set_rate_bps(80_000_000);
        // 10 MB/s for 125 ms, twice, plus 16 packets.
        assert_eq!(media.window(), 2_500_000 + 16 * 1200);
    }

    impl Factory {
        fn build_media(self, mtu: u16) -> Media {
            let b = Arc::new(self).build(Instant::now(), mtu);
            *b.into_any().downcast::<Media>().unwrap()
        }
    }
}
