//! Input over unreliable datagrams, designed as state synchronisation
//! (`docs/design.md` §4).
//!
//! Every packet carries the newest event plus the last few repeated, so a
//! lost packet costs nothing. Snapshots of the held keys, buttons and the
//! pointer go out periodically, and in every packet while anything is held;
//! the receiver compares them with what it has injected and corrects the
//! difference, so a stuck key fixes itself within one snapshot interval.
//! A standalone snapshot repeats the recent events too: a key tapped in a
//! lost packet leaves nothing held for a snapshot to show, and would
//! otherwise never be typed.
//!
//! [`InputSender`] (client) and [`InputReceiver`] (server) hold no clocks or
//! sockets: the caller passes the time and moves the packets.

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

/// Events repeated in each packet.
pub const HISTORY: usize = 16;

/// Standalone snapshots are sent this often, in ms.
pub const SNAPSHOT_INTERVAL_MS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// An evdev key code (`KEY_*`).
    Key { code: u32, pressed: bool },
    /// An evdev button code (`BTN_*`).
    Button { code: u32, pressed: bool },
    /// Absolute position in the output's physical pixels.
    PointerAbs { x: f32, y: f32 },
    /// Relative motion in physical pixels, for pointer capture.
    PointerRel { dx: f32, dy: f32 },
    /// Scroll in physical pixels, plus wheel clicks in 1/120 steps (0 for
    /// smooth scrolling).
    Scroll { dx: f32, dy: f32, v120_x: i32, v120_y: i32 },
}

/// What the sender holds, after the event numbered `InputPacket::seq`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Sorted.
    pub keys: Vec<u32>,
    /// Sorted.
    pub buttons: Vec<u32>,
    /// `None` while the pointer is captured and moves relatively.
    pub pointer: Option<(f32, f32)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputPacket {
    /// Sequence number of the last event in `events`, or of the newest
    /// event sent so far if `events` is empty. The events are consecutive.
    pub seq: u32,
    pub events: Vec<InputEvent>,
    pub snapshot: Option<Snapshot>,
}

/// Client side: numbers events and builds packets.
#[derive(Debug, Default)]
pub struct InputSender {
    seq: u32,
    history: VecDeque<InputEvent>,
    state: State,
    last_snapshot_ms: Option<u64>,
}

impl InputSender {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an event and returns the packet to send now.
    pub fn push(&mut self, event: InputEvent, now_ms: u64) -> InputPacket {
        self.seq = self.seq.wrapping_add(1);
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(event);
        let _ = self.state.apply(&event);
        let snapshot = self.state.anything_held().then(|| self.snapshot(now_ms));
        InputPacket { seq: self.seq, events: self.history.iter().copied().collect(), snapshot }
    }

    /// A standalone snapshot packet, with the recent events, if one is due.
    pub fn tick(&mut self, now_ms: u64) -> Option<InputPacket> {
        let due = self.last_snapshot_ms.is_none_or(|t| now_ms.saturating_sub(t) >= SNAPSHOT_INTERVAL_MS);
        due.then(|| InputPacket {
            seq: self.seq,
            events: self.history.iter().copied().collect(),
            snapshot: Some(self.snapshot(now_ms)),
        })
    }

    /// Forget held keys and buttons without sending releases, as when the
    /// window loses focus; the next snapshot tells the server.
    pub fn release_all(&mut self, now_ms: u64) -> InputPacket {
        self.state.keys.clear();
        self.state.buttons.clear();
        InputPacket { seq: self.seq, events: Vec::new(), snapshot: Some(self.snapshot(now_ms)) }
    }

    fn snapshot(&mut self, now_ms: u64) -> Snapshot {
        self.last_snapshot_ms = Some(now_ms);
        self.state.snapshot()
    }
}

/// Server side: turns packets into the events to inject, exactly once each,
/// and into corrections when a snapshot disagrees with what was injected.
#[derive(Debug, Default)]
pub struct InputReceiver {
    last_seq: Option<u32>,
    state: State,
}

impl InputReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events to inject for `packet`, in order.
    pub fn receive(&mut self, packet: &InputPacket) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let newer = |seq: u32, than: Option<u32>| than.is_none_or(|t| (seq.wrapping_sub(t) as i32) > 0);
        // Out of date. An equal seq still counts: a standalone snapshot
        // repeats the newest seq.
        if !newer(packet.seq, self.last_seq) && Some(packet.seq) != self.last_seq {
            return out;
        }
        let first = packet.seq.wrapping_sub(packet.events.len() as u32).wrapping_add(1);
        for (i, event) in packet.events.iter().enumerate() {
            let seq = first.wrapping_add(i as u32);
            if newer(seq, self.last_seq) && self.state.apply(event) {
                out.push(*event);
            }
        }
        // Even with no events: once a snapshot is applied, events older
        // than it must not be replayed on top of it.
        self.last_seq = Some(packet.seq);
        if let Some(snapshot) = &packet.snapshot {
            self.correct(snapshot, &mut out);
        }
        out
    }

    /// Releases for everything still held, as when the client goes away.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        self.correct(&Snapshot { pointer: self.state.pointer, ..Default::default() }, &mut out);
        out
    }

    fn correct(&mut self, snapshot: &Snapshot, out: &mut Vec<InputEvent>) {
        let want_keys: BTreeSet<u32> = snapshot.keys.iter().copied().collect();
        let want_buttons: BTreeSet<u32> = snapshot.buttons.iter().copied().collect();
        let mut fixes = Vec::new();
        for &code in self.state.keys.difference(&want_keys) {
            fixes.push(InputEvent::Key { code, pressed: false });
        }
        for &code in self.state.buttons.difference(&want_buttons) {
            fixes.push(InputEvent::Button { code, pressed: false });
        }
        if let Some((x, y)) = snapshot.pointer
            && self.state.pointer != Some((x, y))
        {
            fixes.push(InputEvent::PointerAbs { x, y });
        }
        for &code in want_keys.difference(&self.state.keys) {
            fixes.push(InputEvent::Key { code, pressed: true });
        }
        for &code in want_buttons.difference(&self.state.buttons) {
            fixes.push(InputEvent::Button { code, pressed: true });
        }
        for fix in fixes {
            let _ = self.state.apply(&fix);
            out.push(fix);
        }
    }
}

#[derive(Debug, Default)]
struct State {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    pointer: Option<(f32, f32)>,
}

impl State {
    /// Applies `event`; false if it changes nothing (a press of a held key,
    /// say), so that it isn't injected twice.
    fn apply(&mut self, event: &InputEvent) -> bool {
        match *event {
            InputEvent::Key { code, pressed: true } => self.keys.insert(code),
            InputEvent::Key { code, pressed: false } => self.keys.remove(&code),
            InputEvent::Button { code, pressed: true } => self.buttons.insert(code),
            InputEvent::Button { code, pressed: false } => self.buttons.remove(&code),
            InputEvent::PointerAbs { x, y } => {
                self.pointer = Some((x, y));
                true
            }
            InputEvent::PointerRel { .. } => {
                self.pointer = None;
                true
            }
            InputEvent::Scroll { .. } => true,
        }
    }

    fn anything_held(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            keys: self.keys.iter().copied().collect(),
            buttons: self.buttons.iter().copied().collect(),
            pointer: self.pointer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u32 = 30;
    const BTN_LEFT: u32 = 0x110;

    fn key(code: u32, pressed: bool) -> InputEvent {
        InputEvent::Key { code, pressed }
    }

    #[test]
    fn each_event_is_applied_once() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        let p1 = tx.push(InputEvent::PointerRel { dx: 1.0, dy: 0.0 }, 0);
        let p2 = tx.push(InputEvent::PointerRel { dx: 2.0, dy: 0.0 }, 1);
        assert_eq!(rx.receive(&p1).len(), 1);
        assert_eq!(rx.receive(&p2), vec![InputEvent::PointerRel { dx: 2.0, dy: 0.0 }]);
        // Duplicates and stale packets do nothing.
        assert!(rx.receive(&p2).is_empty());
        assert!(rx.receive(&p1).is_empty());
    }

    #[test]
    fn lost_packet_is_recovered_from_history() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        let _lost = tx.push(key(KEY_A, true), 0);
        let p = tx.push(key(KEY_A, false), 1);
        assert_eq!(rx.receive(&p), vec![key(KEY_A, true), key(KEY_A, false)]);
    }

    #[test]
    fn snapshot_releases_a_stuck_key() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        rx.receive(&tx.push(key(KEY_A, true), 0));
        // The release and everything that repeated it are lost.
        for i in 0..HISTORY as u64 + 1 {
            let _ = tx.push(InputEvent::PointerAbs { x: i as f32, y: 0.0 }, i);
        }
        let _lost = tx.push(key(KEY_A, false), 20);
        let snap = tx.tick(200).expect("snapshot due");
        let out = rx.receive(&snap);
        assert!(out.contains(&key(KEY_A, false)), "{out:?}");
    }

    #[test]
    fn snapshot_presses_a_missed_button() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        let mut p = tx.push(InputEvent::Button { code: BTN_LEFT, pressed: true }, 0);
        // Only the snapshot survives.
        p.events.clear();
        assert_eq!(rx.receive(&p), vec![InputEvent::Button { code: BTN_LEFT, pressed: true }]);
    }

    #[test]
    fn snapshot_rides_along_while_held() {
        let mut tx = InputSender::new();
        assert!(tx.push(key(KEY_A, true), 0).snapshot.is_some());
        assert!(tx.push(key(KEY_A, false), 1).snapshot.is_none());
    }

    #[test]
    fn standalone_snapshots_are_rate_limited() {
        let mut tx = InputSender::new();
        assert!(tx.tick(0).is_some());
        assert!(tx.tick(50).is_none());
        assert!(tx.tick(100).is_some());
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut tx = InputSender { seq: u32::MAX - 1, ..Default::default() };
        let mut rx = InputReceiver::new();
        rx.receive(&tx.push(key(KEY_A, true), 0));
        let p = tx.push(key(KEY_A, false), 1);
        assert_eq!(p.seq, 0);
        assert_eq!(rx.receive(&p), vec![key(KEY_A, false)]);
    }

    #[test]
    fn a_tap_lost_before_a_pause_comes_with_the_snapshot() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        rx.receive(&tx.push(InputEvent::PointerAbs { x: 0.0, y: 0.0 }, 0));
        let _lost = tx.push(key(KEY_A, true), 1);
        let _lost = tx.push(key(KEY_A, false), 2);
        // Nothing is held, so only the snapshot's events tell of the tap.
        assert_eq!(rx.receive(&tx.tick(200).unwrap()), vec![key(KEY_A, true), key(KEY_A, false)]);
        // And once only.
        let out = rx.receive(&tx.push(InputEvent::PointerAbs { x: 1.0, y: 0.0 }, 201));
        assert_eq!(out, vec![InputEvent::PointerAbs { x: 1.0, y: 0.0 }]);
    }

    #[test]
    fn release_all_releases_held() {
        let mut tx = InputSender::new();
        let mut rx = InputReceiver::new();
        rx.receive(&tx.push(key(KEY_A, true), 0));
        assert_eq!(rx.release_all(), vec![key(KEY_A, false)]);
        assert!(rx.release_all().is_empty());
    }
}
