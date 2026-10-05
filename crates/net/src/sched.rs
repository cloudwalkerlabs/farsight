//! Datagram priority scheduler (`docs/design.md` §1, "Datagram priority").
//!
//! quinn sends datagrams from one FIFO, so an input packet queued behind a
//! keyframe would wait for all of it. Everything goes through here instead:
//!
//! - strict priority: input and ping > audio > video;
//! - quinn's own buffer is only allowed to hold about one pacing interval of
//!   video, and the rest waits in per-class queues;
//! - video, keyframes included, is paced at a set rate (the congestion
//!   controller's estimate, once there is one), so it never fills the
//!   congestion window in one burst;
//! - only the video queue drops: a keyframe supersedes the frames queued
//!   before it, and a backlog beyond [`MAX_BACKLOG`] drops the oldest frames
//!   that haven't started.
//!
//! [`Queues`] holds the policy and no clocks or sockets; [`Scheduler`] drives
//! it against a quinn connection.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::Connection;
use tokio::sync::Notify;

/// The highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Input, ping and pong.
    Input,
    Audio,
}

/// Video queued beyond this much sending time is dropped.
pub const MAX_BACKLOG: Duration = Duration::from_millis(250);

/// How much video may run ahead of the pacing rate, and how much may sit in
/// quinn's buffer.
const BURST: Duration = Duration::from_millis(2);

/// The burst never goes below this many bytes, so a datagram always fits.
const MIN_BURST: usize = 4 * 1500;

/// How often to look again while quinn's buffer is full; it gives no signal
/// when it drains.
const BUFFER_POLL: Duration = Duration::from_millis(1);

#[derive(Debug)]
struct VideoFrame {
    fragments: VecDeque<Bytes>,
    started: bool,
}

/// What the scheduler would do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Send(Bytes),
    /// Nothing can go before this time; video is waiting for its pace or for
    /// quinn's buffer.
    WaitUntil(Instant),
    Idle,
}

/// Per-class queues and video pacing.
#[derive(Debug)]
pub struct Queues {
    input: VecDeque<Bytes>,
    audio: VecDeque<Bytes>,
    video: VecDeque<VideoFrame>,
    video_bytes: usize,
    rate_bps: u64,
    /// Bytes of video that may go now.
    tokens: f64,
    refilled: Instant,
    dropped_frames: u64,
}

impl Queues {
    pub fn new(rate_bps: u64, now: Instant) -> Self {
        let mut q = Self {
            input: VecDeque::new(),
            audio: VecDeque::new(),
            video: VecDeque::new(),
            video_bytes: 0,
            rate_bps: 1,
            tokens: 0.0,
            refilled: now,
            dropped_frames: 0,
        };
        q.set_rate(rate_bps, now);
        q.tokens = q.burst() as f64;
        q
    }

    pub fn set_rate(&mut self, rate_bps: u64, now: Instant) {
        self.refill(now);
        self.rate_bps = rate_bps.max(8_000);
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    pub fn push(&mut self, priority: Priority, datagram: Bytes) {
        match priority {
            Priority::Input => self.input.push_back(datagram),
            Priority::Audio => self.audio.push_back(datagram),
        }
    }

    /// Queues one video frame's datagrams, in order.
    pub fn push_frame(&mut self, fragments: Vec<Bytes>, keyframe: bool) {
        let bytes: usize = fragments.iter().map(Bytes::len).sum();
        let max = self.bytes_in(MAX_BACKLOG);
        // A keyframe makes everything queued before it useless, and when
        // the backlog is too long the newest picture matters most.
        while let Some(i) = self.video.iter().position(|f| !f.started) {
            if !keyframe && self.video_bytes + bytes <= max {
                break;
            }
            let frame = self.video.remove(i).unwrap();
            self.video_bytes -= frame.fragments.iter().map(Bytes::len).sum::<usize>();
            self.dropped_frames += 1;
        }
        self.video_bytes += bytes;
        self.video.push_back(VideoFrame { fragments: fragments.into(), started: false });
    }

    /// The next datagram to hand to quinn, given how many bytes quinn holds
    /// unsent.
    pub fn next(&mut self, now: Instant, quinn_buffered: usize) -> Next {
        if let Some(d) = self.input.pop_front().or_else(|| self.audio.pop_front()) {
            return Next::Send(d);
        }
        let Some(len) = self.video.front().map(|f| f.fragments.front().map_or(0, Bytes::len)) else {
            return Next::Idle;
        };
        if quinn_buffered + len > self.burst() {
            return Next::WaitUntil(now + BUFFER_POLL);
        }
        self.refill(now);
        if self.tokens < len as f64 {
            let wait = (len as f64 - self.tokens) * 8.0 / self.rate_bps as f64;
            return Next::WaitUntil(now + Duration::from_secs_f64(wait));
        }
        self.tokens -= len as f64;
        let frame = self.video.front_mut().unwrap();
        frame.started = true;
        let d = frame.fragments.pop_front().unwrap_or_default();
        if frame.fragments.is_empty() {
            self.video.pop_front();
        }
        self.video_bytes -= d.len();
        Next::Send(d)
    }

    /// Video frames dropped so far.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// How long the video queued now takes to send at the pacing rate.
    pub fn backlog(&self) -> Duration {
        Duration::from_secs_f64(self.video_bytes as f64 * 8.0 / self.rate_bps as f64)
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate_bps as f64 / 8.0).min(self.burst() as f64);
        self.refilled = now;
    }

    fn burst(&self) -> usize {
        self.bytes_in(BURST).max(MIN_BURST)
    }

    fn bytes_in(&self, d: Duration) -> usize {
        (self.rate_bps as f64 / 8.0 * d.as_secs_f64()) as usize
    }
}

struct Shared {
    queues: Mutex<Queues>,
    wake: Notify,
}

/// Sends datagrams on a connection through [`Queues`]. Cheap to clone; the
/// driver task ends when the connection closes.
#[derive(Clone)]
pub struct Scheduler {
    shared: Arc<Shared>,
}

impl Scheduler {
    /// Starts the driver on the current tokio runtime. `buffer_size` is
    /// quinn's datagram send buffer (`TransportConfig::datagram_send_buffer_size`).
    pub fn spawn(conn: Connection, rate_bps: u64, buffer_size: usize) -> Self {
        let shared =
            Arc::new(Shared { queues: Mutex::new(Queues::new(rate_bps, Instant::now())), wake: Notify::new() });
        tokio::spawn(drive(conn, shared.clone(), buffer_size));
        Self { shared }
    }

    pub fn send(&self, priority: Priority, datagram: Bytes) {
        self.shared.queues.lock().unwrap().push(priority, datagram);
        self.shared.wake.notify_one();
    }

    pub fn send_frame(&self, fragments: Vec<Bytes>, keyframe: bool) {
        self.shared.queues.lock().unwrap().push_frame(fragments, keyframe);
        self.shared.wake.notify_one();
    }

    pub fn set_rate(&self, rate_bps: u64) {
        self.shared.queues.lock().unwrap().set_rate(rate_bps, Instant::now());
        self.shared.wake.notify_one();
    }

    pub fn dropped_frames(&self) -> u64 {
        self.shared.queues.lock().unwrap().dropped_frames()
    }

    pub fn backlog(&self) -> Duration {
        self.shared.queues.lock().unwrap().backlog()
    }
}

async fn drive(conn: Connection, shared: Arc<Shared>, buffer_size: usize) {
    loop {
        let next = loop {
            let buffered = buffer_size.saturating_sub(conn.datagram_send_buffer_space());
            let next = shared.queues.lock().unwrap().next(Instant::now(), buffered);
            let Next::Send(d) = next else { break next };
            match conn.send_datagram(d) {
                Ok(()) => {}
                // Packetized for a larger datagram than the path now takes.
                Err(quinn::SendDatagramError::TooLarge) => tracing::debug!("datagram too large; dropped"),
                Err(err) => {
                    tracing::debug!(%err, "scheduler stopping");
                    return;
                }
            }
        };
        let wake = shared.wake.notified();
        tokio::select! {
            () = wake => {}
            () = sleep_until(&next) => {}
            _ = conn.closed() => return,
        }
    }
}

async fn sleep_until(next: &Next) {
    match next {
        Next::WaitUntil(t) => tokio::time::sleep_until((*t).into()).await,
        _ => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(tag: u8, len: usize) -> Bytes {
        Bytes::from(vec![tag; len])
    }

    fn drain(q: &mut Queues, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        while let Next::Send(b) = q.next(now, 0) {
            out.push(b[0]);
        }
        out
    }

    #[test]
    fn input_and_audio_jump_the_video_queue() {
        let t = Instant::now();
        let mut q = Queues::new(100_000_000, t);
        q.push_frame((0..100).map(|_| d(b'v', 1200)).collect(), true);
        assert_eq!(q.next(t, 0), Next::Send(d(b'v', 1200)));
        q.push(Priority::Audio, d(b'a', 100));
        q.push(Priority::Input, d(b'i', 50));
        assert_eq!(q.next(t, 0), Next::Send(d(b'i', 50)));
        assert_eq!(q.next(t, 0), Next::Send(d(b'a', 100)));
        assert_eq!(q.next(t, 0), Next::Send(d(b'v', 1200)));
    }

    #[test]
    fn video_is_paced() {
        let t = Instant::now();
        // 9.6 Mbps: 1200 bytes per ms, and the burst is MIN_BURST.
        let mut q = Queues::new(9_600_000, t);
        q.push_frame((0..20).map(|_| d(b'v', 1200)).collect(), true);
        let first = drain(&mut q, t).len();
        assert_eq!(first, MIN_BURST / 1200);
        let Next::WaitUntil(at) = q.next(t, 0) else { panic!() };
        assert!(at > t && at <= t + Duration::from_millis(1));
        // 10 ms later, 10 more may go (the burst caps it at 5).
        assert_eq!(drain(&mut q, t + Duration::from_millis(10)).len(), MIN_BURST / 1200);
        assert_eq!(drain(&mut q, t + Duration::from_millis(11)).len(), 1);
    }

    #[test]
    fn quinn_holds_at_most_a_burst_of_video() {
        let t = Instant::now();
        let mut q = Queues::new(1_000_000_000, t);
        q.push_frame(vec![d(b'v', 1200)], false);
        assert!(matches!(q.next(t, q.burst()), Next::WaitUntil(_)));
        // Input still goes.
        q.push(Priority::Input, d(b'i', 10));
        assert_eq!(q.next(t, q.burst()), Next::Send(d(b'i', 10)));
    }

    #[test]
    fn keyframe_supersedes_unstarted_frames() {
        let t = Instant::now();
        let mut q = Queues::new(9_600_000, t);
        q.push_frame((0..10).map(|_| d(1, 1200)).collect(), false);
        assert_eq!(q.next(t, 0), Next::Send(d(1, 1200)));
        q.push_frame(vec![d(2, 1200)], false);
        q.push_frame(vec![d(3, 1200)], true);
        assert_eq!(q.dropped_frames(), 1);
        // The started frame finishes; frame 2 is gone.
        let later = t + Duration::from_secs(1);
        let mut sent = Vec::new();
        for i in 0..20 {
            if let Next::Send(b) = q.next(later + Duration::from_millis(i), 0) {
                sent.push(b[0]);
            }
        }
        assert_eq!(sent, [vec![1; 9], vec![3]].concat());
    }

    #[test]
    fn backlog_drops_the_oldest_frames() {
        let t = Instant::now();
        // 250 ms at 1 Mbps is about 31 KB.
        let mut q = Queues::new(1_000_000, t);
        for i in 0..40 {
            q.push_frame(vec![d(i, 1200)], false);
        }
        assert!(q.dropped_frames() > 10);
        assert!(q.video_bytes <= q.bytes_in(MAX_BACKLOG));
        assert!(q.backlog() <= MAX_BACKLOG && q.backlog() > MAX_BACKLOG / 2);
        let Next::Send(first) = q.next(t, 0) else { panic!() };
        assert!(first[0] > 0);
    }
}
