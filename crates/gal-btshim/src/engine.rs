// SPDX-License-Identifier: GPL-3.0-only
//! Single-worker, sans-I/O AA session. The native adapter owns each returned
//! transmit allocation until its asynchronous completion. only then may it
//! request another chunk. No native pointer is retained here.
use crate::config::Config;
use btsvc_core::aawg::{self, Conn, Outcome};
use std::collections::VecDeque;

const MAX_FRAME: usize = aawg::HEADER_LEN + aawg::RFCOMM_MTU;
const MAX_PENDING: usize = 2 * MAX_FRAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    StaleGeneration,
    NotOpen,
    InvalidFrameSize,
    OversizedFrame,
    QueueFull,
    InvalidConfig,
}

pub struct Engine {
    config: Config,
    conn: Conn,
    generation: u64,
    open: bool,
    rx: Vec<u8>,
    tx: VecDeque<u8>,
}

impl Engine {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            conn: Conn::new(),
            generation: 0,
            open: false,
            rx: Vec::with_capacity(MAX_FRAME),
            tx: VecDeque::with_capacity(MAX_PENDING),
        }
    }

    /// Generations must be nonzero and strictly increasing, including reopen.
    ///
    /// # Errors
    /// Rejects stale generations and invalid credentials.
    pub fn on_open(&mut self, generation: u64) -> Result<(), EngineError> {
        if generation <= self.generation {
            return Err(EngineError::StaleGeneration);
        }
        self.clear();
        self.generation = generation;
        self.config
            .validate()
            .map_err(|_| EngineError::InvalidConfig)?;
        self.open = true;
        let tx = &mut self.tx;
        self.conn
            .on_open(&self.config.as_core(), &mut |frame| tx.extend(frame));
        Ok(())
    }

    fn check(&self, generation: u64) -> Result<(), EngineError> {
        if generation != self.generation {
            Err(EngineError::StaleGeneration)
        } else if !self.open {
            Err(EngineError::NotOpen)
        } else {
            Ok(())
        }
    }

    fn clear(&mut self) {
        self.open = false;
        self.conn.reset();
        self.rx.clear();
        // Queued bytes contain credentials. Do not include them in logs.
        self.tx.clear();
    }

    /// # Errors
    /// Rejects a stale generation or an already closed session.
    pub fn on_close(&mut self, generation: u64) -> Result<(), EngineError> {
        self.check(generation)?;
        self.clear();
        Ok(())
    }

    /// # Errors
    /// Rejects stale/closed sessions, oversized frames and a full transmit queue.
    pub fn on_data(&mut self, generation: u64, bytes: &[u8]) -> Result<Outcome, EngineError> {
        self.check(generation)?;
        let mut outcome = Outcome::Pending;
        // Validate complete frame lengths before handing them to the reused
        // core. Its overflow resync otherwise silently drops a large delivery.
        for &byte in bytes {
            self.rx.push(byte);
            if self.rx.len() < aawg::HEADER_LEN {
                continue;
            }
            let total = aawg::HEADER_LEN + u16::from_be_bytes([self.rx[0], self.rx[1]]) as usize;
            if total > MAX_FRAME {
                self.clear();
                return Err(EngineError::OversizedFrame);
            }
            if self.rx.len() != total {
                continue;
            }
            let tx = &mut self.tx;
            let mut overflow = false;
            let result = self
                .conn
                .on_data(&self.config.as_core(), &self.rx, &mut |frame| {
                    if tx.len() + frame.len() > MAX_PENDING {
                        overflow = true;
                    } else {
                        tx.extend(frame);
                    }
                });
            self.rx.clear();
            if overflow {
                self.clear();
                return Err(EngineError::QueueFull);
            }
            if result == Outcome::Fail {
                self.clear();
                return Ok(Outcome::Fail);
            }
            if result != Outcome::Pending {
                outcome = result;
            }
        }
        Ok(outcome)
    }

    /// Call only with a free native transmit slot. Returned bytes have moved
    /// out of the queue. on submission failure the adapter must close/reset.
    ///
    /// # Errors
    /// Rejects stale/closed sessions and invalid native frame sizes.
    pub fn next_tx(
        &mut self,
        generation: u64,
        frame_size: usize,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        self.check(generation)?;
        if frame_size == 0 || frame_size > u16::MAX as usize {
            return Err(EngineError::InvalidFrameSize);
        }
        if self.tx.is_empty() {
            return Ok(None);
        }
        let n = frame_size.min(self.tx.len());
        Ok(Some(self.tx.drain(..n).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(
            Config::parse(
                "ip=198.51.100.1\nport=5288\nssid=AA\nkey=password\nbssid=02:00:00:00:00:01\n",
            )
            .unwrap(),
        )
    }
    fn frame(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; payload.len() + 4];
        aawg::frame_encode(&mut bytes, id, payload).unwrap();
        bytes
    }
    fn drain(e: &mut Engine, generation: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while let Some(chunk) = e.next_tx(generation, 3).unwrap() {
            assert!(chunk.len() <= 3);
            bytes.extend(chunk);
        }
        bytes
    }

    #[test]
    fn full_handshake_fragmented_rx_and_small_native_packets() {
        let mut e = engine();
        e.on_open(1).unwrap();
        assert_eq!(
            aawg::frame_parse(&drain(&mut e, 1)).unwrap().0,
            aawg::msg::WIFI_START_REQUEST
        );
        for byte in frame(aawg::msg::WIFI_INFO_REQUEST, &[]) {
            assert_eq!(e.on_data(1, &[byte]), Ok(Outcome::Pending));
        }
        let response = drain(&mut e, 1);
        let (id, payload, _) = aawg::frame_parse(&response).unwrap();
        assert_eq!(id, aawg::msg::WIFI_INFO_RESPONSE);
        assert!(response[payload].windows(8).any(|s| s == b"password"));
        let mut incoming = frame(aawg::msg::WIFI_START_RESPONSE, &[]);
        incoming.extend(frame(aawg::msg::WIFI_CONNECT_STATUS, &[8, 0]));
        assert_eq!(e.on_data(1, &incoming), Ok(Outcome::Pass));
        assert_eq!(drain(&mut e, 1), [] as [u8; 0]);
    }

    #[test]
    fn reconnect_discards_old_data_and_rejects_old_completion_generation() {
        let mut e = engine();
        e.on_open(1).unwrap();
        let in_flight = e.next_tx(1, 3).unwrap().unwrap();
        e.on_data(1, &[0, 0]).unwrap();
        e.on_close(1).unwrap();
        e.on_open(2).unwrap();
        assert_eq!(e.on_data(1, &[0, 2]), Err(EngineError::StaleGeneration));
        assert_eq!(e.on_close(1), Err(EngineError::StaleGeneration));
        assert_eq!(e.next_tx(1, 3), Err(EngineError::StaleGeneration));
        assert_eq!(in_flight.len(), 3); // native-owned old packet is unaffected
        assert_eq!(
            aawg::frame_parse(&drain(&mut e, 2)).unwrap().0,
            aawg::msg::WIFI_START_REQUEST
        );
    }

    #[test]
    fn invalid_lengths_fail_closed_and_new_generation_recovers() {
        let mut e = engine();
        e.on_open(1).unwrap();
        assert_eq!(e.next_tx(1, 0), Err(EngineError::InvalidFrameSize));
        assert_eq!(
            e.on_data(1, &[0xff, 0xff, 0, 2]),
            Err(EngineError::OversizedFrame)
        );
        assert_eq!(e.next_tx(1, 10), Err(EngineError::NotOpen));
        assert_eq!(e.on_open(1), Err(EngineError::StaleGeneration));
        e.on_open(2).unwrap();
        // Large coalesced deliveries are processed one bounded frame at a time.
        let incoming = frame(aawg::msg::WIFI_PING_REQUEST, &[]).repeat(1000);
        assert_eq!(e.on_data(2, &incoming), Ok(Outcome::Pending));
        assert_eq!(e.rx, [] as [u8; 0]);
        assert!(e.tx.len() <= MAX_PENDING);
    }

    #[test]
    fn queue_overflow_and_wifi_failure_reset_session() {
        let mut e = engine();
        e.on_open(1).unwrap();
        // Force saturation to exercise the bound, independent of core's
        // current two-response protocol (which cannot fill it normally).
        e.tx.resize(MAX_PENDING, 0);
        assert_eq!(
            e.on_data(1, &frame(aawg::msg::WIFI_INFO_REQUEST, &[])),
            Err(EngineError::QueueFull)
        );
        assert!(e.tx.is_empty());
        e.on_open(2).unwrap();
        e.on_data(2, &frame(aawg::msg::WIFI_INFO_REQUEST, &[]))
            .unwrap();
        e.on_data(2, &frame(aawg::msg::WIFI_START_RESPONSE, &[]))
            .unwrap();
        assert_eq!(
            e.on_data(2, &frame(aawg::msg::WIFI_CONNECT_STATUS, &[8, 1])),
            Ok(Outcome::Fail)
        );
        assert_eq!(e.next_tx(2, 10), Err(EngineError::NotOpen));
    }
}
