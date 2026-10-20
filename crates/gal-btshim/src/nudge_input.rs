// SPDX-License-Identifier: GPL-3.0-only
//! Bounded loopback request input. No socket wait or native operation.
use btsvc_core::wireless_policy::Address;
use std::{
    io::Read,
    net::{Ipv4Addr, TcpListener, TcpStream},
};

struct Client {
    socket: TcpStream,
    line: Vec<u8>,
    started: u64,
}
pub struct Requests {
    listener: TcpListener,
    clients: Vec<Client>,
}

impl Requests {
    /// # Errors
    /// Returns bind/nonblocking errors. an unavailable listener disables nudge.
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            clients: Vec::new(),
        })
    }

    /// Accept at most four sockets and read at most 64 bytes from each per tick.
    #[must_use]
    pub fn poll(&mut self, now: u64) -> [Option<Address>; 4] {
        for _ in 0..4 {
            match self.listener.accept() {
                Ok((socket, _)) if self.clients.len() < 4 => {
                    if socket.set_nonblocking(true).is_ok() {
                        self.clients.push(Client {
                            socket,
                            line: Vec::new(),
                            started: now,
                        });
                    }
                }
                _ => break,
            }
        }
        let mut requests = [None; 4];
        let mut used = 0;
        self.clients.retain_mut(|client| {
            if now.saturating_sub(client.started) > 1000 {
                return false;
            }
            let mut bytes = [0; 64];
            match client.socket.read(&mut bytes) {
                Ok(0) => false,
                Ok(count) => {
                    if client.line.len() + count > 64 {
                        return false;
                    }
                    client.line.extend_from_slice(&bytes[..count]);
                    if client.line.contains(&b'\n') {
                        if let Some(address) = crate::policy::nudge(&client.line) {
                            requests[used] = Some(address);
                            used += 1;
                        }
                        false
                    } else {
                        true
                    }
                }
                Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
            }
        });
        requests
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn client(requests: &Requests) -> TcpStream {
        TcpStream::connect(requests.listener.local_addr().unwrap()).unwrap()
    }
    #[test]
    fn fragmented_command_is_one_owned_address_and_socket_is_released() {
        let mut requests = Requests::bind(0).unwrap();
        let mut socket = client(&requests);
        socket.write_all(b"nudge 01:23:45:").unwrap();
        assert_eq!(requests.poll(1000), [None; 4]);
        socket.write_all(b"67:89:AB\n").unwrap();
        assert_eq!(requests.poll(1001)[0], Some([1, 35, 69, 103, 137, 171]));
        assert!(requests.clients.is_empty());
    }
    #[test]
    fn oversized_incomplete_and_stalled_clients_have_fixed_work_and_lifetime() {
        let mut requests = Requests::bind(0).unwrap();
        let mut socket = client(&requests);
        socket.write_all(&[b'x'; 64]).unwrap();
        assert_eq!(requests.poll(1000), [None; 4]);
        assert_eq!(requests.clients.len(), 1);
        socket.write_all(b"x").unwrap();
        assert_eq!(requests.poll(1001), [None; 4]);
        assert!(requests.clients.is_empty());
        let _held = client(&requests);
        let _ = requests.poll(2000);
        assert_eq!(requests.clients.len(), 1);
        let _ = requests.poll(3001);
        assert!(requests.clients.is_empty());
    }
}
