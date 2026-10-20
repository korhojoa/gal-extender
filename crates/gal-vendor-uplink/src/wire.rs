// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Pure helpers of the uplink, free of gal objects and of the lock, so the
//! host tests reach them directly: the `IoBuffer` bounds decision, the log
//! rate limiter, and the `ServiceDiscoveryResponse` bytes builder.

/// What one inbound `IoBuffer` may forward (`aaup_check_iobuf_bounds`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Bounds {
    /// Forward exactly this many bytes from `data + start` (`end - start`).
    Ok(u32),
    /// Well-formed but zero-length: forward nothing.
    Empty,
    /// Inconsistent metadata: drop, count, log.
    Invalid,
}

/// Validate the data pointer, range order and capacity before forwarding.
/// An empty range forwards no bytes.
#[must_use]
pub fn check_iobuf_bounds(data_non_null: bool, cap: u32, start: u32, end: u32) -> Bounds {
    if !data_non_null || start > end || end > cap {
        return Bounds::Invalid;
    }
    if start == end {
        return Bounds::Empty;
    }
    Bounds::Ok(end - start)
}

/// Rate limiter with power-of-two backoff (`aaup_rl_t`): occurrences 1, 2,
/// 4, 8, ... 4096 are allowed, then about one in 4096.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    hits: u32,
    next: u32,
    step: u32,
}

const RL_MAX: u32 = 4096;

impl RateLimit {
    #[must_use]
    pub const fn new() -> Self {
        RateLimit {
            hits: 0,
            next: 0,
            step: 0,
        }
    }

    /// Count one occurrence. true when it may be logged.
    pub fn allow(&mut self) -> bool {
        self.hits = self.hits.wrapping_add(1);
        if self.hits < self.next {
            return false;
        }
        if self.step == 0 {
            self.step = 1;
        } else if self.step < RL_MAX {
            self.step *= 2;
        }
        self.next = self.hits.wrapping_add(self.step);
        true
    }
}

/// Default `VendorExtensionService.service_name` (env `AAUP_SERVICE_NAME`).
pub const SERVICE_NAME_DEFAULT: &[u8] = b"org.korho.gal.tunnel";
/// Default `package_white_list` entry (env `AAUP_PACKAGE`).
pub const PACKAGE_DEFAULT: &[u8] = b"org.korho.aatunnel";

/// A bounded protobuf writer over a fixed buffer.
struct Pb<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl Pb<'_> {
    fn byte(&mut self, b: u8) -> Option<()> {
        *self.buf.get_mut(self.at)? = b;
        self.at += 1;
        Some(())
    }

    fn varint(&mut self, mut v: u32) -> Option<()> {
        while v >= 0x80 {
            #[allow(clippy::cast_possible_truncation)] // the low 7 bits + flag
            self.byte((v | 0x80) as u8)?;
            v >>= 7;
        }
        #[allow(clippy::cast_possible_truncation)] // v < 0x80
        self.byte(v as u8)
    }

    fn lendelim(&mut self, field: u32, data: &[u8]) -> Option<()> {
        self.varint((field << 3) | 2)?;
        self.varint(u32::try_from(data.len()).ok()?)?;
        let dst = self.buf.get_mut(self.at..self.at + data.len())?;
        dst.copy_from_slice(data);
        self.at += data.len();
        Some(())
    }
}

#[must_use]
pub fn build_vendor_sdr(
    out: &mut [u8],
    channel_id: u8,
    service_name: &[u8],
    package: &[u8],
) -> u32 {
    if service_name.len() > 100 || package.len() > 100 {
        return 0;
    }
    let mut ves_buf = [0u8; 256];
    let mut ves = Pb {
        buf: &mut ves_buf,
        at: 0,
    };
    let mut svc_buf = [0u8; 320];
    let mut svc = Pb {
        buf: &mut svc_buf,
        at: 0,
    };
    let inner = (|| {
        ves.lendelim(1, service_name)?;
        if !package.is_empty() {
            ves.lendelim(2, package)?;
        }
        svc.byte(1 << 3)?; // Service.id (1, varint)
        svc.varint(u32::from(channel_id))?;
        let vn = ves.at;
        svc.lendelim(12, &ves.buf[..vn])
    })();
    if inner.is_none() {
        return 0;
    }
    let sn = svc.at;
    if sn + 8 > out.len() {
        return 0;
    }
    let mut o = Pb { buf: out, at: 0 };
    match o.lendelim(1, &svc.buf[..sn]) {
        Some(()) => u32::try_from(o.at).unwrap_or(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_is_power_of_two() {
        let mut rl = RateLimit::new();
        let allowed: Vec<u32> = (1..=40).filter(|_| rl.allow()).collect();
        assert_eq!(allowed, [1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn sdr_bytes_encode_the_vendor_channel() {
        let mut out = [0u8; 512];
        let n = build_vendor_sdr(&mut out, 16, SERVICE_NAME_DEFAULT, PACKAGE_DEFAULT) as usize;
        let mut ves = vec![0x0a, 20];
        ves.extend_from_slice(SERVICE_NAME_DEFAULT);
        ves.extend_from_slice(&[0x12, 18]);
        ves.extend_from_slice(PACKAGE_DEFAULT);
        let mut svc = vec![0x08, 16, 0x62, u8::try_from(ves.len()).unwrap()];
        svc.extend_from_slice(&ves);
        let mut want = vec![0x0a, u8::try_from(svc.len()).unwrap()];
        want.extend_from_slice(&svc);
        assert_eq!(&out[..n], &want[..]);
    }

    #[test]
    fn sdr_limits() {
        let mut out = [0u8; 512];
        assert_eq!(build_vendor_sdr(&mut out, 1, &[b'a'; 101], b""), 0);
        assert_eq!(build_vendor_sdr(&mut out, 1, b"a", &[b'p'; 101]), 0);
        // No package: no field 2.
        let n = build_vendor_sdr(&mut out, 200, b"svc", b"") as usize;
        assert_eq!(
            &out[..n],
            &[0x0a, 10, 0x08, 0xc8, 0x01, 0x62, 5, 0x0a, 3, b's', b'v', b'c']
        );
        // Too small an output: refused, not cut.
        let mut small = [0u8; 16];
        assert_eq!(build_vendor_sdr(&mut small, 1, b"abcdef", b""), 0);
    }
}
