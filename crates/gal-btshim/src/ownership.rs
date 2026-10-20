// SPDX-License-Identifier: GPL-3.0-only
//! One native packet may be owned by RFCOMM at a time, across disconnects.
#[derive(Default)]
pub struct Flight {
    pending: Option<(usize, u64)>,
}
impl Flight {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn submit(&mut self, packet: usize, generation: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("native packet still owned");
        }
        self.pending = Some((packet, generation));
        Ok(())
    }
    pub fn complete(&mut self, packet: usize) -> Result<u64, &'static str> {
        match self.pending {
            Some((expected, generation)) if expected == packet => {
                self.pending = None;
                Ok(generation)
            }
            _ => Err("unmatched packet completion"),
        }
    }
    pub fn rejected(&mut self, packet: usize) -> Result<(), &'static str> {
        self.complete(packet).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_cannot_reuse_old_native_packet() {
        let mut flight = Flight::default();
        flight.submit(123, 1).unwrap();
        assert!(flight.submit(123, 2).is_err());
        assert!(flight.complete(456).is_err());
        assert!(flight.busy());
        assert_eq!(flight.complete(123).unwrap(), 1);
        flight.submit(123, 2).unwrap();
        assert_eq!(flight.complete(123).unwrap(), 2);
        assert!(flight.complete(123).is_err());
    }
}
