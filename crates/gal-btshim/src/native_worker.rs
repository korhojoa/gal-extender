// SPDX-License-Identifier: GPL-3.0-only
//! Original guarded worker ownership with bounded additive PPS inputs.
use super::nudge::Nudge;
use super::{
    call1, call2, callback, initialized, listed, log, peer, put16, put32, wall_ms, Event, Flight,
    Guard, Outcome, Session, Snapshot, Storage, CHANNEL, ENVELOPE, EVENTS, FRAME, HOOKED,
    NATIVE_READY, REQUESTED, RF_ACCEPT, RF_BASE, RF_CLOSE, RF_CREDIT, RF_DEREGISTER, RF_REGISTER,
    RF_SEND, RF_SIZE, SDP_ADD, SDP_HEAD, SDP_REMOVE, STOP,
};
use std::{
    ffi::c_void,
    ptr,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

struct Inputs {
    live: Option<btsvc_core::wireless_policy::Policy>,
    admission: Option<crate::policy_input::Admission>,
    revoke: bool,
}

fn inputs(session: &mut Session, reader: &mut crate::policy_input::AdmissionReader) -> Inputs {
    if session.take_clear() && !STOP.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(crate::policy::SESSION_PPS);
    }
    let now = wall_ms();
    let policy_text = crate::pps_io::read(crate::policy::POLICY_PPS);
    let credentials = if STOP.load(Ordering::Acquire) {
        None
    } else {
        crate::pps_io::read(aawg_creds::CREDS_PPS)
    };
    let live = policy_text
        .as_deref()
        .and_then(|text| crate::policy::parse(text, now));
    let admission = reader.read(&Snapshot::new(policy_text, credentials), now, &mut log);
    let revoke = session.grant().is_some() && !session.retains(live.as_ref());
    if revoke && !STOP.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(crate::policy::SESSION_PPS);
    }
    Inputs {
        live,
        admission,
        revoke,
    }
}

#[derive(PartialEq, Eq)]
enum Registration {
    Empty,
    RfOnly,
    Published,
}

struct Worker {
    storage: &'static mut Storage,
    engine: Session,
    credentials: crate::policy_input::AdmissionReader,
    flight: Flight,
    registration: Registration,
    open: bool,
    closing: bool,
    generation: u64,
    handshake_done: bool,
    opened: Instant,
    nudge: Nudge,
    requests: Option<crate::nudge_input::Requests>,
}

impl Worker {
    fn new() -> Self {
        let port = std::env::var("AAWG_NUDGE_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .unwrap_or(5289);
        let requests = crate::nudge_input::Requests::bind(port).ok();
        log(&format!(
            "nudge listener port={port} available={}",
            requests.is_some()
        ));
        Self {
            storage: Storage::allocate(),
            engine: Session::default(),
            credentials: crate::policy_input::AdmissionReader::default(),
            flight: Flight::default(),
            registration: Registration::Empty,
            open: false,
            closing: false,
            generation: 0,
            handshake_done: false,
            opened: Instant::now(),
            nudge: Nudge::new(),
            requests,
        }
    }
    fn channel(&self) -> *mut c_void {
        self.storage.channel.pointer()
    }
    fn outputs(&self, input: &Inputs, last_status: &mut u64, last_session: &mut u64) -> bool {
        use crate::status::{acknowledge, session_due, Readiness};
        let state = if !HOOKED.load(Ordering::Acquire) {
            Readiness::MissingHook
        } else if STOP.load(Ordering::Acquire) || !NATIVE_READY.load(Ordering::Acquire) {
            Readiness::Unready
        } else if self.registration != Registration::Published {
            Readiness::Unregistered
        } else {
            Readiness::Published
        };
        if !matches!(state, Readiness::Published) {
            return false;
        }
        let now = wall_ms();
        let periodic = now.saturating_sub(*last_status) >= 1000;
        if let Some(policy) = input.live.as_ref() {
            if periodic {
                *last_status = now;
                let _ = acknowledge(state, policy.revision, now, |text| {
                    crate::pps_io::write("/ramdisk/pps/aawg/bt_status", text)
                });
            }
            if STOP.load(Ordering::Acquire) {
                return false;
            }
            if self.engine.retains(Some(policy))
                && session_due(self.engine.passed, self.engine.id, *last_session, periodic)
            {
                if let Some((address, epoch)) = self.engine.grant() {
                    let text = format!("version::1\nepoch::{epoch}\nupdated_ms::{now}\npeer::{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}\nsession::{}\nend::1\n",
                        address[0],address[1],address[2],address[3],address[4],address[5],self.engine.id);
                    if crate::pps_io::write(crate::policy::SESSION_PPS, &text).is_err() {
                        let _ = std::fs::remove_file(crate::policy::SESSION_PPS);
                        return true;
                    }
                    *last_session = self.engine.id;
                }
            }
        }
        false
    }
    unsafe fn close(&mut self) {
        self.closing = true;
        call1(RF_CLOSE, self.channel());
    }
    unsafe fn register(&mut self) -> bool {
        let channel = self.channel();
        put32(channel, 8, callback as *const () as usize);
        put16(
            channel,
            12,
            u16::try_from(FRAME).expect("bounded native frame"),
        );
        CHANNEL.store(channel as usize, Ordering::Release);
        let register: unsafe extern "C" fn(*mut c_void, *mut u8, u8) -> i32 =
            std::mem::transmute(RF_REGISTER);
        let status = register(channel, self.storage.number.get(), 4);
        if status != 0 {
            log(&format!(
                "RFCOMM registration rejected: {status:#x}; no retry"
            ));
            return false;
        }
        self.registration = Registration::RfOnly;
        let number = ptr::read_volatile(self.storage.number.get());
        if self.storage.record.set_channel(number).is_err() {
            log("invalid allocated channel; disabled");
            return false;
        }
        let status = call1(SDP_ADD, self.storage.record.record_ptr());
        if status != 0 {
            log(&format!("SDP registration rejected: {status:#x}; rollback"));
            return false;
        }
        self.registration = Registration::Published;
        log(&format!(
            "AA service published on factory RFCOMM channel {number}; awaiting phone"
        ));
        true
    }
    unsafe fn intact(&self) -> bool {
        let number = ptr::read_volatile(self.storage.number.get());
        self.storage.intact()
            && (1..=6).contains(&number)
            && listed(SDP_HEAD, self.storage.record.record_ptr() as usize)
            && listed(
                RF_BASE + (usize::from(number) - 1) * 16,
                self.channel() as usize,
            )
    }
    unsafe fn opened(&mut self, input: &Inputs) {
        if self.open
            || self.flight.busy()
            || !self.engine.retains(input.live.as_ref())
            || peer(self.channel()) != self.engine.grant().map(|(address, _)| address)
        {
            self.close();
            return;
        }
        self.generation += 1;
        if self.engine.on_open(self.generation).is_err() {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.handshake_done = false;
        self.opened = Instant::now();
        log("phone opened native AA channel; bootstrap starting");
    }
    unsafe fn data(&mut self, bytes: &[u8]) {
        if !self.open || self.closing {
            return;
        }
        match self.engine.on_data(self.generation, bytes) {
            Ok(Outcome::Pass) => {
                self.handshake_done = true;
                log("phone reports WiFi joined; bootstrap complete");
            }
            Ok(Outcome::Pending) => {}
            _ => {
                let _ = self.engine.on_close(self.generation);
                self.engine.revoke();
                self.close();
                log("bootstrap failed; closing own AA channel");
            }
        }
        if !self.closing {
            call2(RF_CREDIT, self.channel(), 1);
        }
    }
    unsafe fn event(&mut self, event: &Event, input: &mut Inputs) {
        match *event {
            Event::NudgeReady(number) => self.nudge.ready(number, input.live.as_ref()),
            Event::NudgeOpen => self.nudge.opened(),
            Event::NudgeQueryFailed => self.nudge.query_failed(),
            Event::NudgeChannelClosed => self.nudge.channel_closed(),
            Event::Incoming(address) => {
                // The native parameter selects acceptance.
                let accept = !self.open
                    && !self.closing
                    && !self.flight.busy()
                    && input
                        .admission
                        .take()
                        .is_some_and(|grant| self.engine.admit(grant, address, wall_ms()));
                let status = call2(RF_ACCEPT, self.channel(), usize::from(accept));
                if accept && status != 0 && status != 2 {
                    self.engine.revoke();
                    log(&format!("accept rejected: {status:#x}"));
                }
            }
            Event::Open => self.opened(input),
            Event::Closing => {
                self.closing = true;
                let _ = self.engine.on_close(self.generation);
            }
            Event::Closed => {
                self.open = false;
                self.closing = false;
                let _ = self.engine.on_close(self.generation);
                if self.flight.busy() {
                    log("closed with unreturned packet; storage quarantined");
                    STOP.store(true, Ordering::Release);
                }
                log("native AA channel closed");
            }
            Event::Data { len, bytes } => self.data(&bytes[..len]),
            Event::Handled { packet, status } => match self.flight.complete(packet) {
                Ok(epoch) if epoch == self.generation => {
                    if status != 0 && self.open && !self.closing {
                        self.close();
                    }
                }
                _ => {
                    log("unexpected packet completion; disabling");
                    STOP.store(true, Ordering::Release);
                }
            },
        }
    }
    unsafe fn events(&mut self, input: &mut Inputs) {
        // Never retain the queue mutex across a native call: callbacks may
        // synchronously reenter even when an operation returns pending.
        for _ in 0..32 {
            if STOP.load(Ordering::Acquire) {
                break;
            }
            let event = if let Ok(mut queue) = EVENTS.lock() {
                queue.pop()
            } else {
                STOP.store(true, Ordering::Release);
                None
            };
            let Some(event) = event else {
                break;
            };
            self.event(&event, input);
        }
    }
    unsafe fn transmit(&mut self) {
        if !self.open || self.closing || self.flight.busy() || STOP.load(Ordering::Acquire) {
            return;
        }
        let size = call1(RF_SIZE, self.channel());
        let Ok(size) = usize::try_from(size) else {
            return;
        };
        if size == 0 {
            return;
        }
        match self.engine.next_tx(self.generation, size.min(FRAME)) {
            Ok(Some(bytes)) => {
                let packet = self.storage.packet.pointer();
                ptr::write_bytes(self.storage.packet.data.get().cast::<u8>(), 0, ENVELOPE);
                ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.storage.payload.get().cast::<u8>(),
                    bytes.len(),
                );
                put32(packet, 8, self.storage.payload.get() as usize);
                put16(
                    packet,
                    12,
                    u16::try_from(bytes.len()).expect("bounded transmit chunk"),
                );
                if self
                    .flight
                    .submit(packet as usize, self.generation)
                    .is_err()
                {
                    STOP.store(true, Ordering::Release);
                    return;
                }
                let status = call2(RF_SEND, self.channel(), packet as usize);
                if status != 2 {
                    let _ = self.flight.rejected(packet as usize);
                    self.close();
                    log(&format!("native send not queued: {status:#x}"));
                }
            }
            Ok(None) => {}
            Err(_) => self.close(),
        }
    }
    unsafe fn cleanup(&mut self) {
        // Preserve original post-STOP trylock and membership/canary guards.
        if let Some(_guard) = Guard::try_acquire() {
            if initialized() && self.storage.intact() {
                self.nudge.cleanup();
                if self.registration == Registration::Published
                    && listed(SDP_HEAD, self.storage.record.record_ptr() as usize)
                {
                    call1(SDP_REMOVE, self.storage.record.record_ptr());
                }
                let number = ptr::read_volatile(self.storage.number.get());
                if self.registration != Registration::Empty
                    && (1..=6).contains(&number)
                    && listed(
                        RF_BASE + (usize::from(number) - 1) * 16,
                        self.channel() as usize,
                    )
                {
                    if self.open || self.closing {
                        call1(RF_CLOSE, self.channel());
                    }
                    let status = call2(
                        RF_DEREGISTER,
                        self.channel(),
                        self.storage.number.get() as usize,
                    );
                    log(&format!(
                        "AA worker stopped; deregister={status:#x}; storage retained"
                    ));
                }
            }
        }
    }
}

pub(super) fn run() {
    let mut worker = Worker::new();
    let started = Instant::now();
    let mut last_status = 0;
    let mut last_session = 0;
    let mut publication_failed = false;
    loop {
        if STOP.load(Ordering::Acquire) {
            break;
        }
        if worker.registration == Registration::Empty
            && started.elapsed() > Duration::from_secs(120)
        {
            log("readiness timeout (requires observed native destructor hook); disabled");
            break;
        }
        let mut input = inputs(&mut worker.engine, &mut worker.credentials);
        input.revoke |= publication_failed;
        let requests = worker
            .requests
            .as_mut()
            .map_or([None; 4], |requests| requests.poll(wall_ms()));
        if STOP.load(Ordering::Acquire) {
            break;
        }
        let guard = unsafe { Guard::try_acquire() };
        let Some(guard) = guard else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        unsafe {
            if !initialized() {
                NATIVE_READY.store(false, Ordering::Release);
                drop(guard);
                if worker.registration != Registration::Empty {
                    log("factory stack deinitialized; retaining objects, no re-registration");
                    break;
                }
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            NATIVE_READY.store(true, Ordering::Release);
            if worker.registration == Registration::Empty {
                if !REQUESTED.load(Ordering::Acquire) {
                    drop(guard);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                if !worker.register() {
                    break;
                }
            }
            if !worker.intact() {
                log("native registration/storage changed; disabled, storage retained");
                break;
            }
            if input.revoke {
                if worker.open && !worker.closing {
                    worker.close();
                }
                worker.engine.revoke();
                log("policy/session unavailable; own bootstrap revoked");
            }
            if !worker.open && !worker.closing {
                if let Some(policy) = input.live.as_ref() {
                    for address in requests.into_iter().flatten() {
                        worker.nudge.start(address, policy);
                    }
                }
            }
            worker.events(&mut input);
            if STOP.load(Ordering::Acquire) {
                break;
            }
            if worker.open
                && !worker.closing
                && !worker.handshake_done
                && worker.opened.elapsed() > Duration::from_secs(30)
            {
                let _ = worker.engine.on_close(worker.generation);
                worker.close();
                log("bootstrap timeout; closed own AA channel");
            }
            worker.transmit();
        }
        drop(guard);
        publication_failed = worker.outputs(&input, &mut last_status, &mut last_session);
        thread::sleep(Duration::from_millis(10));
    }
    STOP.store(true, Ordering::Release);
    NATIVE_READY.store(false, Ordering::Release);
    unsafe {
        worker.cleanup();
    }
}
