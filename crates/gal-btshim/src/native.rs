// SPDX-License-Identifier: GPL-3.0-only
//! Audited ARM/QNX boundary. All native allocations are deliberately retained
//! for process lifetime, including after failure/reset. No controller takeover.
use crate::{
    events::{Event, Queue, FRAME},
    identity,
    ownership::Flight,
    policy_input::Snapshot,
    sdp,
    session::Session,
};
use btsvc_core::aawg::Outcome;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
    thread,
};

const BT: usize = 0x2d_5914;
const GUARD: usize = 0x2d_54dc;
const OSAPI: usize = 0x2d_5684;
const OSAPI_DROP: usize = 0x27_acd4;
const READY: usize = OSAPI + 0x16;
const SDP_HEAD: usize = BT + 0x1b_b184;
const RF_BASE: usize = BT + 0x1b_f1c8;
const RF_REGISTER: usize = 0x27_7a30;
const RF_DEREGISTER: usize = 0x27_79cc;
const RF_ACCEPT: usize = 0x27_7354;
const RF_CLOSE: usize = 0x27_7410;
const RF_CREDIT: usize = 0x27_7174;
const RF_SEND: usize = 0x27_7210;
const RF_SIZE: usize = 0x27_708c;
const SDP_ADD: usize = 0x20_725c;
const SDP_REMOVE: usize = 0x20_71dc;
const CANARY: u8 = 0xa7;
// Deliberately larger than observed native access bounds (channel 0xad,
// packet 0x62). The guarded allocation size is not a native structure-size claim.
const ENVELOPE: usize = 4096;

static VERIFIED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static HOOKED: AtomicBool = AtomicBool::new(false);
static EXITED: AtomicBool = AtomicBool::new(true);
static REAL_ATEXIT: AtomicUsize = AtomicUsize::new(0);
static CHANNEL: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicBool = AtomicBool::new(false);
static NATIVE_READY: AtomicBool = AtomicBool::new(false);
static EVENTS: Mutex<Queue> = Mutex::new(Queue::new());
static WORKER: Mutex<Option<thread::JoinHandle<()>>> = Mutex::new(None);

extern "C" {
    fn pthread_mutex_trylock(mutex: *mut c_void) -> i32;
    fn pthread_mutex_unlock(mutex: *mut c_void) -> i32;
}

type RegisterFn = unsafe extern "C" fn(*mut c_void) -> usize;
// Preserve the existing gal deployment's opaque service-registration forwarder.
static REGISTER_ORIGINAL: qnx_dl::NextFn<RegisterFn> =
    unsafe { qnx_dl::NextFn::new(c"_ZN4comm19ServiceRegistration15registerServiceEv") };

#[export_name = "_ZN4comm19ServiceRegistration15registerServiceEv"]
pub unsafe extern "C" fn service_registered(this: *mut c_void) -> usize {
    let Some(original) = REGISTER_ORIGINAL.get() else {
        return usize::MAX;
    };
    let status = original(this);
    if status == 0 && VERIFIED.load(Ordering::Acquire) {
        let _ = gal_btshim_start();
    }
    status
}

#[no_mangle]
pub extern "C" fn gal_btshim_start() -> i32 {
    crate::startup::request(
        &REQUESTED,
        VERIFIED.load(Ordering::Acquire),
        STOP.load(Ordering::Acquire),
        NATIVE_READY.load(Ordering::Acquire),
    )
}

fn log(message: &str) {
    qnx_slog::log::raw(
        qnx_slog::sys::SLOG_INFO,
        format_args!("[GalBtShim] ms={} {message}\n", qnx_slog::sys::now_ms()),
    );
}

/// The next definition of `name` after this library (`dlsym(RTLD_NEXT)`,
/// QNX handle -3) as an address, or 0.
fn next_symbol(name: &core::ffi::CStr) -> usize {
    qnx_dl::sym_next(name) as usize
}

/// Forward all registrations unchanged except this exact
/// verified singleton's destructor. It is registered by the executable through
/// its PLT after construction. Refuse native activation if this was not observed.
#[no_mangle]
pub unsafe extern "C" fn __aeabi_atexit(
    object: *mut c_void,
    destructor: usize,
    dso: *mut c_void,
) -> i32 {
    let mut real = REAL_ATEXIT.load(Ordering::Acquire);
    if real == 0 {
        real = next_symbol(c"__aeabi_atexit");
        if real == 0 || real == __aeabi_atexit as *const () as usize {
            return -1;
        }
        REAL_ATEXIT.store(real, Ordering::Release);
    }
    let register: unsafe extern "C" fn(*mut c_void, usize, *mut c_void) -> i32 =
        std::mem::transmute(real);
    let wrap =
        VERIFIED.load(Ordering::Acquire) && object as usize == OSAPI && destructor == OSAPI_DROP;
    let result = register(
        object,
        if wrap {
            osapi_drop as *const () as usize
        } else {
            destructor
        },
        dso,
    );
    if wrap && result == 0 {
        HOOKED.store(true, Ordering::Release);
    }
    result
}

unsafe extern "C" fn osapi_drop(object: *mut c_void) {
    // Runs before the real destructor touches/frees the native mutex. The
    // worker uses trylock and contains no socket I/O, so it can stop here.
    STOP.store(true, Ordering::Release);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let handle = WORKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = handle {
            let _ = worker.join();
        }
    }));
    while !EXITED.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let drop: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(OSAPI_DROP);
    drop(object);
}

pub extern "C" fn initialize() {
    if catch_unwind(AssertUnwindSafe(initialize_inner)).is_err() {
        STOP.store(true, Ordering::Release);
        log("constructor panic contained; disabled");
    }
}

fn initialize_inner() {
    if std::env::var("GAL_NATIVE_BT").as_deref() != Ok("1") {
        return;
    }
    let result = (|| -> Result<(), String> {
        // Resolve the chain before permitting destructor interception.
        let real = next_symbol(c"__aeabi_atexit");
        if real == 0 || real == __aeabi_atexit as *const () as usize {
            return Err("cannot chain __aeabi_atexit".into());
        }
        REAL_ATEXIT.store(real, Ordering::Release);
        // File identity alone is insufficient: these must be the actual
        // main-executable symbols, with the audited mapped instructions.
        for (name, address) in [
            (c"bt", BT),
            (c"HfRfCallback", 0x24_8208),
            (c"_ZN5OSAPID1Ev", OSAPI_DROP),
        ] {
            if qnx_dl::sym_default(name) as usize != address {
                return Err("not the pinned factory btstack process".into());
            }
        }
        // An installation can place the identical executable elsewhere. The override
        // changes only the file to hash. mapped symbol and instruction checks
        // above/below remain mandatory for both audited file identities.
        let executable = std::env::var("GAL_BTSHIM_EXECUTABLE")
            .unwrap_or_else(|_| "/eso/bin/apps/btstack".into());
        identity::verify_file(&executable).map_err(|e| format!("firmware identity: {e}"))?;
        unsafe {
            for entry in identity::ENTRIES {
                let mut bytes = [0u8; 16];
                for (index, byte) in bytes.iter_mut().enumerate() {
                    *byte = ptr::read_volatile((entry.address + index) as *const u8);
                }
                if !identity::matches_fingerprint(&bytes, entry.fingerprint) {
                    return Err(format!("mapped entry mismatch: {}", entry.name));
                }
            }
            let mut bytes = [0u8; 16];
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = ptr::read_volatile((OSAPI_DROP + index) as *const u8);
            }
            if !identity::matches_fingerprint(
                &bytes,
                "df29f0f5d8d8d8ae4180527b03fe110ffffe3e400880bbacd1ef8c57540205cc",
            ) {
                return Err("mapped destructor mismatch".into());
            }
        }
        VERIFIED.store(true, Ordering::Release);
        EXITED.store(false, Ordering::Release);
        let worker = thread::Builder::new()
            .name("native-aa".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(run)).is_err() {
                    STOP.store(true, Ordering::Release);
                    log("worker panic contained; native allocations retained");
                }
                EXITED.store(true, Ordering::Release);
            })
            .map_err(|_| {
                EXITED.store(true, Ordering::Release);
                "cannot start AA worker"
            })?;
        *WORKER.lock().map_err(|_| "worker handle poisoned")? = Some(worker);
        log(concat!(
            "Rust bootstrap loaded; waiting for factory readiness; build=",
            env!("CARGO_PKG_VERSION")
        ));
        Ok(())
    })();
    if let Err(error) = result {
        STOP.store(true, Ordering::Release);
        log(&format!("disabled: {error}"));
    }
}

#[repr(C, align(16))]
struct Opaque {
    data: UnsafeCell<[u8; ENVELOPE + 64]>,
}
impl Opaque {
    fn new() -> Self {
        let mut bytes = [0; ENVELOPE + 64];
        bytes[ENVELOPE..].fill(CANARY);
        Self {
            data: UnsafeCell::new(bytes),
        }
    }
    fn pointer(&self) -> *mut c_void {
        self.data.get().cast()
    }
    fn intact(&self) -> bool {
        (ENVELOPE..ENVELOPE + 64)
            .all(|i| unsafe { ptr::read_volatile(self.data.get().cast::<u8>().add(i)) == CANARY })
    }
}

struct Storage {
    channel: Opaque,
    packet: Opaque,
    payload: UnsafeCell<[u8; FRAME]>,
    number: UnsafeCell<u8>,
    record: sdp::Record,
}
impl Storage {
    fn allocate() -> &'static mut Self {
        Box::leak(Box::new(Self {
            channel: Opaque::new(),
            packet: Opaque::new(),
            payload: UnsafeCell::new([0; FRAME]),
            number: UnsafeCell::new(0),
            record: sdp::Record::new(1).expect("constant channel"),
        }))
    }
    fn intact(&self) -> bool {
        self.channel.intact() && self.packet.intact()
    }
}

unsafe fn read32(address: usize) -> usize {
    ptr::read_volatile(address as *const u32) as usize
}
unsafe fn read8(address: usize) -> u8 {
    ptr::read_volatile(address as *const u8)
}
unsafe fn put16(base: *mut c_void, offset: usize, value: u16) {
    ptr::write_unaligned(base.cast::<u8>().add(offset).cast::<u16>(), value);
}
unsafe fn put32(base: *mut c_void, offset: usize, value: usize) {
    ptr::write_unaligned(
        base.cast::<u8>().add(offset).cast::<u32>(),
        u32::try_from(value).expect("pinned 32-bit ABI"),
    );
}

struct Guard(*mut c_void);
impl Guard {
    unsafe fn try_acquire() -> Option<Self> {
        if !HOOKED.load(Ordering::Acquire) || read8(GUARD) & 1 == 0 {
            return None;
        }
        let mutex = read32(OSAPI + 4) as *mut c_void;
        if mutex.is_null() || pthread_mutex_trylock(mutex) != 0 {
            return None;
        }
        Some(Self(mutex))
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            pthread_mutex_unlock(self.0);
        }
    }
}

unsafe fn initialized() -> bool {
    if read8(READY) == 0
        || read32(SDP_HEAD) == 0
        || read32(SDP_HEAD + 4) == 0
        || read32(SDP_HEAD) == SDP_HEAD
    {
        return false;
    }
    let mut occupied = false;
    for i in 0..6 {
        let slot = RF_BASE + i * 16;
        if read32(slot) == 0 || read32(slot + 4) == 0 {
            return false;
        }
        occupied |= read8(slot + 13) & 1 != 0 && read32(slot) != slot;
    }
    occupied
}

// List membership is inspected only under the native lock. Stop at a bounded
// number of native records. never follow our own stale links after reinit.
unsafe fn listed(head: usize, object: usize) -> bool {
    let mut next = read32(head);
    for _ in 0..256 {
        if next == object {
            return true;
        }
        if next == head || next == 0 || next & 3 != 0 {
            return false;
        }
        next = read32(next);
    }
    false
}

unsafe fn call1(address: usize, object: *mut c_void) -> i32 {
    let call: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(address);
    call(object)
}
unsafe fn call2(address: usize, object: *mut c_void, argument: usize) -> i32 {
    let call: unsafe extern "C" fn(*mut c_void, usize) -> i32 = std::mem::transmute(address);
    call(object, argument)
}

/// Callback never calls factory APIs, logs, allocates or performs socket I/O.
/// Synchronous reentry from a worker's native call can therefore only enqueue.
unsafe extern "C" fn callback(channel: *mut c_void, parameters: *mut c_void) {
    if STOP.load(Ordering::Acquire) {
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        if channel as usize != CHANNEL.load(Ordering::Acquire) || parameters.is_null() {
            STOP.store(true, Ordering::Release);
            return;
        }
        let p = parameters as usize;
        let event = match read8(p) {
            // Copy peer identity while callback storage is valid. never queue
            // the borrowed native remote pointer.
            1 => {
                if let Some(address) = remote_address(read32(p + 12)) {
                    Event::Incoming(address)
                } else {
                    STOP.store(true, Ordering::Release);
                    return;
                }
            }
            2 => Event::Open,
            3 => Event::Closing,
            4 => Event::Closed,
            5 => {
                let len = ptr::read_unaligned((p + 8) as *const u16) as usize;
                let source = read32(p + 12) as *const u8;
                if len > FRAME || (source.is_null() && len != 0) {
                    STOP.store(true, Ordering::Release);
                    return;
                }
                let mut bytes = [0; FRAME];
                if len != 0 {
                    ptr::copy_nonoverlapping(source, bytes.as_mut_ptr(), len);
                }
                Event::Data { len, bytes }
            }
            6 => Event::Handled {
                packet: read32(p + 12),
                status: ptr::read_unaligned((p + 2) as *const u16),
            },
            _ => return,
        };
        match EVENTS.lock() {
            Ok(mut queue) => {
                if !queue.push(event) {
                    STOP.store(true, Ordering::Release);
                }
            }
            Err(_) => {
                STOP.store(true, Ordering::Release);
            }
        }
    }));
    if result.is_err() {
        STOP.store(true, Ordering::Release);
    }
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| {
            u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
        })
}

unsafe fn remote_address(remote: usize) -> Option<btsvc_core::wireless_policy::Address> {
    if remote == 0 {
        return None;
    }
    let mut address = [0; 6];
    ptr::copy_nonoverlapping((remote + 19) as *const u8, address.as_mut_ptr(), 6);
    address.reverse();
    Some(address)
}

unsafe fn peer(channel: *mut c_void) -> Option<btsvc_core::wireless_policy::Address> {
    let get: unsafe extern "C" fn(*mut c_void) -> usize = std::mem::transmute(0x27_6f8c_usize);
    remote_address(get(channel))
}

#[path = "native_nudge.rs"]
mod nudge;
#[path = "native_worker.rs"]
mod worker;
use worker::run;
