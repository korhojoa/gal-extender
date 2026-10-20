// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Android Auto navigation-channel decode: `NavigationState` (message id
//! 0x8006) and `NavigationCurrentPosition` (0x8007), the modern (protocol
//! >= 1.6) messages that carry the destination address, ETA and turn steps.
//!
//! Pure bytes-in: the caller hands the protobuf BODY (the frame minus its
//! 2-byte message id) and this fills a [`NavState`]. Field layout is
//! aa-proxy-rs `protos.proto` `NavigationState`/`NavigationCurrentPosition`;
//! confirmed on-car (VW MIB2). `no_std`, no alloc, no panic: every
//! access is `get`/`locate`, strings land in fixed buffers.
//!
//! The two messages own disjoint fields, so each `update_*` clears only what it
//! owns and refills from the new message. a field absent from the current
//! message clears (the HMI treats missing as unknown).

use protobuf_wire::{varint, wire};

/// Longest string field we keep. Destination addresses and road names are
/// short. anything longer is truncated (still valid key=value output).
const S_ROAD: usize = 96;
const S_ADDR: usize = 160;
const S_ETA: usize = 24;

/// One decoded snapshot of the navigation state, accumulated across a 0x8006
/// and a 0x8007. `None`/`len==0` means "not present in the latest messages".
pub struct NavState {
    // from NavigationState (0x8006), steps[0] + destinations[0]
    pub maneuver: Option<i64>,
    pub roundabout_exit: Option<i64>,
    pub roundabout_angle: Option<i64>,
    road: [u8; S_ROAD],
    road_len: usize,
    addr: [u8; S_ADDR],
    addr_len: usize,
    // from NavigationCurrentPosition (0x8007)
    pub step_dist_m: Option<i64>,
    pub time_to_step_s: Option<i64>,
    pub dest_dist_m: Option<i64>,
    eta: [u8; S_ETA],
    eta_len: usize,
    pub time_to_arrival_s: Option<i64>,
}

impl Default for NavState {
    fn default() -> Self {
        NavState::new()
    }
}

impl NavState {
    #[must_use]
    pub const fn new() -> Self {
        NavState {
            maneuver: None,
            roundabout_exit: None,
            roundabout_angle: None,
            road: [0; S_ROAD],
            road_len: 0,
            addr: [0; S_ADDR],
            addr_len: 0,
            step_dist_m: None,
            time_to_step_s: None,
            dest_dist_m: None,
            eta: [0; S_ETA],
            eta_len: 0,
            time_to_arrival_s: None,
        }
    }

    /// Clear everything (session teardown / route ended).
    pub fn reset(&mut self) {
        *self = NavState::new();
    }

    #[must_use]
    pub fn road(&self) -> &[u8] {
        self.road.get(..self.road_len).unwrap_or(&[])
    }
    #[must_use]
    pub fn address(&self) -> &[u8] {
        self.addr.get(..self.addr_len).unwrap_or(&[])
    }
    #[must_use]
    pub fn eta(&self) -> &[u8] {
        self.eta.get(..self.eta_len).unwrap_or(&[])
    }

    /// Decode a `NavigationState` (0x8006) body into the 0x8006-owned fields.
    /// `steps[0].maneuver{type,roundabout_exit_number,roundabout_exit_angle}`,
    /// `steps[0].road.name`, `destinations[0].address`.
    pub fn update_from_nav_state(&mut self, body: &[u8]) {
        self.maneuver = locate_varint(body, &[1, 1, 1]);
        self.roundabout_exit = locate_varint(body, &[1, 1, 2]);
        self.roundabout_angle = locate_varint(body, &[1, 1, 3]);
        self.road_len = copy_str(&mut self.road, locate_str(body, &[1, 2, 1]));
        self.addr_len = copy_str(&mut self.addr, locate_str(body, &[2, 1]));
    }

    /// Decode a `NavigationCurrentPosition` (0x8007) body into the 0x8007-owned
    /// fields. `step_distance.distance.meters`,
    /// `step_distance.time_to_step_seconds`,
    /// `destination_distances[0].distance.meters`, `.estimated_time_at_arrival`,
    /// `.time_to_arrival_seconds`.
    pub fn update_from_current_position(&mut self, body: &[u8]) {
        self.step_dist_m = locate_varint(body, &[1, 1, 1]);
        self.time_to_step_s = locate_varint(body, &[1, 2]);
        self.dest_dist_m = locate_varint(body, &[2, 1, 1]);
        self.eta_len = copy_str(&mut self.eta, locate_str(body, &[2, 2]));
        self.time_to_arrival_s = locate_varint(body, &[2, 3]);
    }

    /// Render the decoded nav fields as `key=value` lines into `out` (the
    /// caller prepends `seq=` and `state=` and does the atomic write). Only
    /// present fields are emitted. Returns bytes written, or None if `out` is
    /// too small.
    pub fn render(&self, out: &mut [u8]) -> Option<usize> {
        let mut n = 0usize;
        if let Some(v) = self.maneuver {
            kv_int(out, &mut n, b"step_maneuver", v)?;
        }
        if let Some(v) = self.roundabout_exit {
            kv_int(out, &mut n, b"roundabout_exit", v)?;
        }
        if let Some(v) = self.roundabout_angle {
            kv_int(out, &mut n, b"roundabout_angle", v)?;
        }
        if self.road_len > 0 {
            kv_str(out, &mut n, b"step_road", self.road())?;
        }
        if self.addr_len > 0 {
            kv_str(out, &mut n, b"dest_address", self.address())?;
        }
        if let Some(v) = self.step_dist_m {
            kv_int(out, &mut n, b"step_dist_m", v)?;
        }
        if let Some(v) = self.time_to_step_s {
            kv_int(out, &mut n, b"time_to_step_s", v)?;
        }
        if let Some(v) = self.dest_dist_m {
            kv_int(out, &mut n, b"dest_dist_m", v)?;
        }
        if self.eta_len > 0 {
            kv_str(out, &mut n, b"eta", self.eta())?;
        }
        if let Some(v) = self.time_to_arrival_s {
            kv_int(out, &mut n, b"time_to_arrival_s", v)?;
        }
        Some(n)
    }
}

/// Locate a scalar varint field by path and read it as a signed int.
fn locate_varint(body: &[u8], path: &[u32]) -> Option<i64> {
    let (off, _len) = wire::locate(body, path, wire::VARINT)?;
    let (v, _) = varint::read(body.get(off..)?)?;
    // protobuf int32/int64 land in the two's-complement bit pattern. reinterpret.
    Some(v.cast_signed())
}

/// Locate a length-delimited (string) field by path. returns its bytes.
fn locate_str<'a>(body: &'a [u8], path: &[u32]) -> Option<&'a [u8]> {
    let (off, len) = wire::locate(body, path, wire::LEN)?;
    body.get(off..off.checked_add(len)?)
}

/// Copy `src` (if any) into `dst`, sanitising bytes that would break the
/// `key=value` line format (newline/CR -> space, and drop anything below a
/// space or above 0x7e to keep the file plain-ASCII). Returns the length.
fn copy_str(dst: &mut [u8], src: Option<&[u8]>) -> usize {
    let Some(src) = src else { return 0 };
    let mut n = 0usize;
    for &b in src {
        let Some(slot) = dst.get_mut(n) else { break };
        *slot = if (0x20..=0x7e).contains(&b) { b } else { b' ' };
        n += 1;
    }
    n
}

fn kv_int(out: &mut [u8], n: &mut usize, key: &[u8], v: i64) -> Option<()> {
    protobuf_wire::write::bytes(out, n, key)?;
    protobuf_wire::write::bytes(out, n, b"=")?;
    write_i64(out, n, v)?;
    protobuf_wire::write::bytes(out, n, b"\n")
}

fn kv_str(out: &mut [u8], n: &mut usize, key: &[u8], val: &[u8]) -> Option<()> {
    protobuf_wire::write::bytes(out, n, key)?;
    protobuf_wire::write::bytes(out, n, b"=")?;
    protobuf_wire::write::bytes(out, n, val)?;
    protobuf_wire::write::bytes(out, n, b"\n")
}

/// Decimal `i64` into `out` at `*n`, no alloc. Handles negatives and `i64::MIN`.
fn write_i64(out: &mut [u8], n: &mut usize, v: i64) -> Option<()> {
    if v < 0 {
        protobuf_wire::write::bytes(out, n, b"-")?;
    }
    // work in u64 magnitude to survive i64::MIN
    let mut mag = v.unsigned_abs();
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i = i.checked_sub(1)?;
        *buf.get_mut(i)? = b'0' + (mag % 10) as u8;
        mag /= 10;
        if mag == 0 {
            break;
        }
    }
    protobuf_wire::write::bytes(out, n, buf.get(i..)?)
}

/// Map a modern `NavigationState` `ManeuverType` (0..50, direction baked into
/// the type) to gal's OLD coarse `NextTurnEvent` pair `(event, turn_side)` used
/// by `CDSIJob_UpdateNavigationNextTurnEvent`. `event` is `NextTurnEnum`
/// (0..19), `turn_side` is `TurnSide` (1=LEFT, 2=RIGHT, 3=UNSPECIFIED).
/// An unknown modern
/// value maps to `(0 UNKNOWN, 3)`.
#[must_use]
pub const fn map_maneuver(m: i64) -> (i32, i32) {
    // NextTurnEnum: 1 DEPART,2 NAME_CHANGE,3 SLIGHT_TURN,4 TURN,5 SHARP_TURN,
    // 6 U_TURN,7 ON_RAMP,8 OFF_RAMP,9 FORK,10 MERGE,11 RA_ENTER,12 RA_EXIT,
    // 13 RA_ENTER_AND_EXIT,14 STRAIGHT,16 FERRY_BOAT,17 FERRY_TRAIN,19 DESTINATION
    match m {
        1 => (1, 3),                 // DEPART
        2 => (2, 3),                 // NAME_CHANGE
        3 | 25 => (9, 1),            // KEEP_LEFT -> FORK, left; FORK_LEFT
        4 | 26 => (9, 2),            // KEEP_RIGHT -> FORK, right; FORK_RIGHT
        5 => (3, 1),                 // TURN_SLIGHT_LEFT
        6 => (3, 2),                 // TURN_SLIGHT_RIGHT
        7 => (4, 1),                 // TURN_NORMAL_LEFT
        8 => (4, 2),                 // TURN_NORMAL_RIGHT
        9 => (5, 1),                 // TURN_SHARP_LEFT
        10 => (5, 2),                // TURN_SHARP_RIGHT
        11 => (6, 1),                // U_TURN_LEFT
        12 => (6, 2),                // U_TURN_RIGHT
        13 | 15 | 17 | 19 => (7, 1), // ON_RAMP_* LEFT variants
        14 | 16 | 18 | 20 => (7, 2), // ON_RAMP_* RIGHT variants
        21 | 23 => (8, 1),           // OFF_RAMP_* LEFT
        22 | 24 => (8, 2),           // OFF_RAMP_* RIGHT
        27 => (10, 1),               // MERGE_LEFT
        28 => (10, 2),               // MERGE_RIGHT
        29 => (10, 3),               // MERGE_SIDE_UNSPECIFIED
        32..=35 => (13, 3),          // ROUNDABOUT_ENTER_AND_EXIT_{CW,CCW}[_WITH_ANGLE]
        36 => (14, 3),               // STRAIGHT
        37 => (16, 3),               // FERRY_BOAT
        38 => (17, 3),               // FERRY_TRAIN
        39 | 40 => (19, 3),          // DESTINATION / DESTINATION_STRAIGHT
        41 => (19, 1),               // DESTINATION_LEFT
        42 => (19, 2),               // DESTINATION_RIGHT
        43 | 45 => (11, 3),          // ROUNDABOUT_ENTER_{CW,CCW}
        44 | 46 => (12, 3),          // ROUNDABOUT_EXIT_{CW,CCW}
        47 => (16, 1),               // FERRY_BOAT_LEFT
        48 => (16, 2),               // FERRY_BOAT_RIGHT
        _ => (0, 3),                 // UNKNOWN / unmapped
    }
}

/// Longest road name a [`Turn`] carries (the decoder's road buffer).
pub const TURN_ROAD: usize = S_ROAD;

/// The fields of gal's native `CDSIJob_UpdateNavigationNextTurn{Event,
/// Distance}` jobs for one decoded state, mapped to gal's OLD
/// `NextTurnEnum`/`TurnSide` ([`map_maneuver`]). The event job is posted when
/// `has_turn`, the distance job when `has_distance`. `road` is the turn's
/// target road name (not the destination address).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    /// `NextTurnEnum` (0..19).
    pub event: i32,
    /// `TurnSide` (1 left, 2 right, 3 unspecified).
    pub turn_side: i32,
    /// Roundabout exit angle, degrees.
    pub turn_angle: i32,
    /// Roundabout exit number.
    pub turn_number: i32,
    /// Step distance in meters (distance job).
    pub distance_m: i32,
    pub time_to_step_s: i32,
    pub has_turn: bool,
    pub has_distance: bool,
    pub road_len: usize,
    pub road: [u8; TURN_ROAD],
}

fn clip_i32(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

impl NavState {
    /// The turn-job fields for the current state. A road name with no
    /// maneuver is still a postable turn event.
    #[must_use]
    pub fn turn(&self) -> Turn {
        let mut t = Turn {
            event: 0,
            turn_side: 3,
            turn_angle: self.roundabout_angle.map_or(0, clip_i32),
            turn_number: self.roundabout_exit.map_or(0, clip_i32),
            distance_m: 0,
            time_to_step_s: 0,
            has_turn: false,
            has_distance: false,
            road_len: 0,
            road: [0; TURN_ROAD],
        };
        if let Some(m) = self.maneuver {
            let (event, side) = map_maneuver(m);
            t.event = event;
            t.turn_side = side;
            t.has_turn = true;
        }
        let mut n = 0usize;
        for (d, &s) in t.road.iter_mut().zip(self.road()) {
            *d = s;
            n += 1;
        }
        t.road_len = n;
        if n > 0 {
            t.has_turn = true;
        }
        if let Some(d) = self.step_dist_m {
            t.distance_m = clip_i32(d);
            t.time_to_step_s = self.time_to_step_s.map_or(0, clip_i32);
            t.has_distance = true;
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A length as the one-byte varint these short test messages use.
    fn len8(n: usize) -> u8 {
        u8::try_from(n).unwrap()
    }

    // Build a minimal NavigationState: steps[0]{ maneuver{type=8}, road{name="Main St"} },
    // destinations[0]{ address="1234 Example Parkway" }.
    fn nav_state_bytes() -> std::vec::Vec<u8> {
        // maneuver{ type(1)=8 } = 08 08
        let maneuver = [0x08u8, 0x08];
        // road{ name(1)="Main St" } = 0a 07 "Main St"
        let mut road = std::vec![0x0a, 7];
        road.extend_from_slice(b"Main St");
        // step{ maneuver(1)=..., road(2)=... }
        let mut step = std::vec![];
        step.push(0x0a);
        step.push(len8(maneuver.len()));
        step.extend_from_slice(&maneuver);
        step.push(0x12);
        step.push(len8(road.len()));
        step.extend_from_slice(&road);
        // destination{ address(1)="1234 Example Parkway" }
        let addr = b"1234 Example Parkway";
        let mut dest = std::vec![0x0a, len8(addr.len())];
        dest.extend_from_slice(addr);
        // NavigationState{ steps(1)=step, destinations(2)=dest }
        let mut msg = std::vec![];
        msg.push(0x0a);
        msg.push(len8(step.len()));
        msg.extend_from_slice(&step);
        msg.push(0x12);
        msg.push(len8(dest.len()));
        msg.extend_from_slice(&dest);
        msg
    }

    // NavigationCurrentPosition: step_distance{ distance{ meters=120 }, time_to_step_seconds=15 },
    // destination_distances[0]{ distance{ meters=8801 }, estimated_time_at_arrival="10:59",
    // time_to_arrival_seconds=540 }.
    fn cur_pos_bytes() -> std::vec::Vec<u8> {
        // distance{ meters(1)=120 } = 08 78
        let dist_step = [0x08u8, 0x78];
        // step_distance{ distance(1)=..., time_to_step_seconds(2)=15 }
        let mut sd = std::vec![0x0a, len8(dist_step.len())];
        sd.extend_from_slice(&dist_step);
        sd.push(0x10);
        sd.push(15);
        // distance{ meters(1)=8801 } = 08 e1 44
        let dist_dest = [0x08u8, 0xe1, 0x44];
        // destination_distance{ distance(1)=..., eta(2)="10:59", tta(3)=540 }
        let eta = b"10:59";
        let mut dd = std::vec![0x0a, len8(dist_dest.len())];
        dd.extend_from_slice(&dist_dest);
        dd.push(0x12);
        dd.push(len8(eta.len()));
        dd.extend_from_slice(eta);
        dd.push(0x18);
        dd.push(0x9c);
        dd.push(0x04); // 540 as varint = 9c 04
        let mut msg = std::vec![0x0a, len8(sd.len())];
        msg.extend_from_slice(&sd);
        msg.push(0x12);
        msg.push(len8(dd.len()));
        msg.extend_from_slice(&dd);
        msg
    }

    #[test]
    fn turn_fields_match_expected_mapping() {
        let mut s = NavState::new();
        let t = s.turn();
        assert!(!t.has_turn && !t.has_distance);
        assert_eq!((t.event, t.turn_side, t.road_len), (0, 3, 0));
        s.update_from_nav_state(&nav_state_bytes());
        s.update_from_current_position(&cur_pos_bytes());
        let t = s.turn();
        // maneuver 8 = TURN_NORMAL_RIGHT -> (TURN, RIGHT)
        assert_eq!((t.event, t.turn_side), (4, 2));
        assert!(t.has_turn && t.has_distance);
        assert_eq!(&t.road[..t.road_len], b"Main St");
        assert_eq!((t.distance_m, t.time_to_step_s), (120, 15));
    }

    #[test]
    fn decodes_nav_state_fields() {
        let mut s = NavState::new();
        s.update_from_nav_state(&nav_state_bytes());
        assert_eq!(s.maneuver, Some(8));
        assert_eq!(s.road(), b"Main St");
        assert_eq!(s.address(), b"1234 Example Parkway");
    }

    #[test]
    fn decodes_current_position_fields() {
        let mut s = NavState::new();
        s.update_from_current_position(&cur_pos_bytes());
        assert_eq!(s.step_dist_m, Some(120));
        assert_eq!(s.time_to_step_s, Some(15));
        assert_eq!(s.dest_dist_m, Some(8801));
        assert_eq!(s.eta(), b"10:59");
        assert_eq!(s.time_to_arrival_s, Some(540));
    }

    #[test]
    fn renders_key_value() {
        let mut s = NavState::new();
        s.update_from_nav_state(&nav_state_bytes());
        s.update_from_current_position(&cur_pos_bytes());
        let mut out = [0u8; 512];
        let n = s.render(&mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(text.contains("dest_address=1234 Example Parkway\n"));
        assert!(text.contains("step_maneuver=8\n"));
        assert!(text.contains("step_road=Main St\n"));
        assert!(text.contains("dest_dist_m=8801\n"));
        assert!(text.contains("eta=10:59\n"));
        assert!(text.contains("time_to_arrival_s=540\n"));
        assert!(text.contains("step_dist_m=120\n"));
    }

    #[test]
    fn absent_fields_and_garbage_are_safe() {
        let mut s = NavState::new();
        // truncated / junk never panics, yields nothing
        for n in 0..40 {
            let b = nav_state_bytes();
            s.update_from_nav_state(b.get(..n).unwrap_or(&[]));
            let _ = cur_pos_bytes();
        }
        s.reset();
        let mut out = [0u8; 512];
        assert_eq!(s.render(&mut out), Some(0));
    }

    #[test]
    fn maneuver_mapping() {
        assert_eq!(map_maneuver(1), (1, 3)); // DEPART
        assert_eq!(map_maneuver(5), (3, 1)); // TURN_SLIGHT_LEFT -> SLIGHT_TURN, LEFT
        assert_eq!(map_maneuver(8), (4, 2)); // TURN_NORMAL_RIGHT -> TURN, RIGHT
        assert_eq!(map_maneuver(11), (6, 1)); // U_TURN_LEFT
        assert_eq!(map_maneuver(41), (19, 1)); // DESTINATION_LEFT
        assert_eq!(map_maneuver(44), (12, 3)); // ROUNDABOUT_EXIT_CW
        assert_eq!(map_maneuver(36), (14, 3)); // STRAIGHT
        assert_eq!(map_maneuver(999), (0, 3)); // unmapped -> UNKNOWN
    }

    #[test]
    fn newline_in_string_is_sanitised() {
        // address with an embedded newline must not break the key=value format
        let addr = b"A\nB";
        let mut dest = std::vec![0x0a, len8(addr.len())];
        dest.extend_from_slice(addr);
        let mut msg = std::vec![0x12, len8(dest.len())];
        msg.extend_from_slice(&dest);
        let mut s = NavState::new();
        s.update_from_nav_state(&msg);
        assert_eq!(s.address(), b"A B");
    }
}
