// SPDX-License-Identifier: GPL-3.0-only
//! CVS1 transport for the selected cluster-video sink. Only the existing
//! cluster writer thread calls this module. gal's receive thread never connects.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const PORT: u16 = 5320;
/// The companion decoder is in the sibling DIO component directory.
///
/// # Errors
/// Rejects relative directories and a root without a component parent.
pub fn helper_path(loader_dir: &Path) -> io::Result<PathBuf> {
    if !loader_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative loader directory",
        ));
    }
    let parent = loader_dir.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "loader directory has no component parent",
        )
    })?;
    Ok(parent.join("dio-extender/cluster-video"))
}
const WRITE_BUDGET: Duration = Duration::from_millis(250);
const START_BUDGET: Duration = Duration::from_secs(5);

/// The socket belongs to one AA session. A broken connection cannot continue
/// a P chain on another decoder: the next connection starts at an IDR.
pub struct Writer {
    socket: Option<TcpStream>,
    need_idr: bool,
    started: bool,
    port: u16,
}

impl Writer {
    #[must_use]
    pub const fn new(port: u16) -> Self {
        Self {
            socket: None,
            need_idr: true,
            started: false,
            port,
        }
    }

    pub fn invalidate(&mut self) {
        self.socket = None;
        self.need_idr = true;
        self.started = false;
    }

    /// Send one complete Annex-B access unit. `start` is called at most once
    /// after a failed connection. it must detach the helper without a shell.
    /// False means waiting for an IDR, never an accepted output frame.
    ///
    /// # Errors
    /// Returns invalid-input, helper-start, connection or bounded write errors.
    pub fn send(
        &mut self,
        au: &[u8],
        idr: bool,
        cancelled: impl Fn() -> bool,
        mut start: impl FnMut(u16) -> io::Result<()>,
    ) -> io::Result<bool> {
        if au.is_empty() || au.len() > 4 * 1024 * 1024 {
            self.invalidate();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid CVS1 AU size",
            ));
        }
        if cancelled() || (self.need_idr && !idr) {
            return Ok(false);
        }
        if self.socket.is_none() {
            let until = Instant::now() + START_BUDGET;
            loop {
                if cancelled() {
                    return Ok(false);
                }
                match connect(self.port) {
                    Ok(socket) => {
                        self.socket = Some(socket);
                        break;
                    }
                    Err(error) => {
                        if !self.started {
                            self.started = true;
                            start(self.port)?;
                        }
                        if Instant::now() >= until {
                            return Err(error);
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        let len = u32::try_from(au.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut header = [0; 8];
        header[..4].copy_from_slice(&len.to_le_bytes());
        header[4..].copy_from_slice(&u32::from(idr).to_le_bytes());
        let until = Instant::now() + WRITE_BUDGET;
        let socket = self.socket.as_mut().ok_or(io::ErrorKind::NotConnected)?;
        let result = write_until(socket, &header, until, &cancelled)
            .and_then(|()| write_until(socket, au, until, &cancelled));
        if let Err(error) = result {
            self.invalidate();
            return Err(error);
        }
        self.need_idr = false;
        Ok(true)
    }
}

fn connect(port: u16) -> io::Result<TcpStream> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut socket = TcpStream::connect_timeout(&addr, Duration::from_millis(100))?;
    socket.set_nodelay(true)?;
    // QNX can report connect success for a refused port. The magic write is
    // part of the connection attempt, as in DIO's maintained sender.
    write_until(&mut socket, b"CVS1", Instant::now() + WRITE_BUDGET, &|| {
        false
    })?;
    Ok(socket)
}

fn write_until(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    until: Instant,
    cancelled: &impl Fn() -> bool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let left = until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        socket.set_write_timeout(Some(left))?;
        match socket.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => bytes = &bytes[count..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Launch only the normal packaged helper. No descriptor other than /dev/null
/// crosses into it, and NOZOMBIE avoids SIGCHLD disrupting gal's USB reads.
///
/// # Errors
/// Returns a path, descriptor, signal-mask or native spawn error.
#[cfg(target_os = "nto")]
pub fn start(path: &Path, port: u16) -> io::Result<i32> {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::fd::AsRawFd;
    extern "C" {
        fn spawn(
            path: *const c_char,
            count: c_int,
            map: *const c_int,
            inherit: *const c_void,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> c_int;
    }
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let port = CString::new(format!("port={port}"))?;
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            key != "LD_PRELOAD" && key != "LD_LIBRARY_PATH" && key != "IPL_CONFIG_DIR"
        })
        .filter_map(|(key, value)| {
            CString::new(format!(
                "{}={}",
                key.to_string_lossy(),
                value.to_string_lossy()
            ))
            .ok()
        })
        .collect();
    env.push(CString::new("LD_LIBRARY_PATH=/lib:/mnt/app/root/lib-target:/eso/lib:/mnt/app/usr/lib:/mnt/app/armle/lib:/mnt/app/armle/lib/dll:/mnt/app/armle/usr/lib")?);
    env.push(CString::new("IPL_CONFIG_DIR=/etc/eso/production")?);
    let mut envp: Vec<_> = env.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    let argv = [path.as_ptr(), port.as_ptr(), std::ptr::null()];
    let mut inherit = [0_u32; 64];
    inherit[0] = 0x200 | 0x2000; // QNX spawn.h: SETSID | NOZOMBIE, DIO's contract.
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let map = [null.as_raw_fd(); 3];
    let empty = [0; 2];
    let mut old = [0; 2];
    // The writer blocks signals. Give the detached helper a normal mask.
    // SAFETY: QNX sigset_t is two u32 words. 2 is SIG_SETMASK.
    if unsafe { crate::sys::sig::pthread_sigmask(2, &raw const empty, &raw mut old) } != 0 {
        return Err(io::Error::other("cannot prepare helper signal mask"));
    }
    // SAFETY: vectors live for spawn and have terminating null pointers.
    let pid = unsafe {
        spawn(
            path.as_ptr(),
            3,
            map.as_ptr(),
            inherit.as_ptr().cast(),
            argv.as_ptr(),
            envp.as_ptr(),
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: restore only this writer thread's original mask.
    let restored =
        unsafe { crate::sys::sig::pthread_sigmask(2, &raw const old, std::ptr::null_mut()) };
    if restored != 0 {
        return Err(io::Error::other("cannot restore writer signal mask"));
    }
    if pid < 0 {
        Err(error)
    } else {
        Ok(pid)
    }
}

#[cfg(not(target_os = "nto"))]
/// Host builds never launch a QNX helper.
///
/// # Errors
/// Always returns `Unsupported` on a host build.
pub fn start(_path: &Path, _port: u16) -> io::Result<i32> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    fn capture(listener: TcpListener, expected: usize) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut bytes = vec![0; expected];
            socket.read_exact(&mut bytes).unwrap();
            bytes
        })
    }

    #[test]
    fn helper_follows_the_selected_loader_directory() {
        for (loader, expected) in [
            (
                "/fs/sda0/gal-extender",
                "/fs/sda0/dio-extender/cluster-video",
            ),
            (
                "/fs/sdb0/gal-extender",
                "/fs/sdb0/dio-extender/cluster-video",
            ),
            (
                "/internal/custom/gal-extender",
                "/internal/custom/dio-extender/cluster-video",
            ),
            (
                "/qnx/owned override/gal",
                "/qnx/owned override/dio-extender/cluster-video",
            ),
        ] {
            assert_eq!(helper_path(Path::new(loader)).unwrap(), Path::new(expected));
        }
        for invalid in ["", ".", "relative/gal", "/"] {
            assert_eq!(
                helper_path(Path::new(invalid)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn cvs_magic_lengths_flags_and_idr_gate_are_exact() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let read = capture(listener, 4 + 8 + 5 + 8 + 5);
        let mut writer = Writer::new(port);
        let idr = [0, 0, 0, 1, 0x65];
        let p = [0, 0, 0, 1, 0x41];
        let no_start = |_| panic!("listener already running");
        assert!(!writer.send(&p, false, || false, no_start).unwrap());
        assert!(writer.send(&idr, true, || false, no_start).unwrap());
        assert!(writer.send(&p, false, || false, no_start).unwrap());
        let got = read.join().unwrap();
        assert_eq!(&got[..4], b"CVS1");
        assert_eq!(&got[4..12], &[5, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&got[12..17], &idr);
        assert_eq!(&got[17..25], &[5, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&got[25..], &p);
    }

    #[test]
    fn loss_requires_a_new_idr_and_a_new_session_magic() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut writer = Writer::new(port);
        let au = [0, 0, 1, 0x65];
        let reader = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = [0; 16];
                socket.read_exact(&mut bytes).unwrap();
                assert_eq!(&bytes[..4], b"CVS1");
                assert_eq!(&bytes[12..], &au);
            }
        });
        assert!(writer
            .send(&au, true, || false, |_| unreachable!())
            .unwrap());
        writer.invalidate();
        assert!(!writer
            .send(&au, false, || false, |_| unreachable!())
            .unwrap());
        assert!(writer
            .send(&au, true, || false, |_| unreachable!())
            .unwrap());
        reader.join().unwrap();
    }

    #[test]
    fn cancelled_or_invalid_input_does_not_start_a_helper() {
        let mut writer = Writer::new(0);
        assert!(!writer
            .send(
                &[0, 0, 1, 0x65],
                true,
                || true,
                |_| panic!("cancelled helper")
            )
            .unwrap());
        assert!(writer
            .send(&[], true, || false, |_| unreachable!())
            .is_err());
        assert!(writer
            .send(&vec![0; (4 << 20) + 1], true, || false, |_| unreachable!())
            .is_err());
    }

    #[test]
    fn absent_receiver_starts_one_helper_then_delivers_the_idr() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut reader = None;
        let mut starts = 0;
        let mut writer = Writer::new(port);
        assert!(writer
            .send(
                &[0, 0, 1, 0x65],
                true,
                || false,
                |port| {
                    starts += 1;
                    reader = Some(capture(TcpListener::bind((Ipv4Addr::LOCALHOST, port))?, 16));
                    Ok(())
                }
            )
            .unwrap());
        assert_eq!(starts, 1);
        assert_eq!(&reader.unwrap().join().unwrap()[..4], b"CVS1");
    }
}
