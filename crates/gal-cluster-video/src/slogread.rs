// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa
//! Owns the shared encoder observer from `mpe-encoder-state`.
use crate::gate;
use crate::log::{cl_log, cl_loge};
use mpe_encoder_state::{Config, Observer};
use std::sync::{Arc, OnceLock};

static OBSERVER: OnceLock<Option<Observer>> = OnceLock::new();
fn diagnostic(text: &str) {
    cl_loge!("[GalCluster] {text}");
}
fn observer() -> Option<&'static Observer> {
    OBSERVER.get().and_then(Option::as_ref)
}

pub fn start() -> bool {
    OBSERVER
        .get_or_init(|| {
            match Observer::start(Config {
                object: gate::MPE_STATE.path_str().into(),
                prefix: "galmark ".into(),
                clock: qnx_time::mono_ms,
                emit_marker: Arc::new(|text| qnx_slog::log::text_code(gate::NV_MAJOR, 5, text)),
                log: diagnostic,
            }) {
                Ok(observer) => {
                    cl_log!(
                        "[GalCluster] owned encoder observer started; current evidence pending"
                    );
                    Some(observer)
                }
                Err(error) => {
                    cl_loge!(
                        "[GalCluster] encoder observer unavailable: {error}; writes stay blocked"
                    );
                    None
                }
            }
        })
        .is_some()
}
pub fn mark() -> bool {
    observer().is_some_and(Observer::begin)
}
pub fn level() -> Option<bool> {
    observer().and_then(Observer::level)
}
pub fn invalidate_generation() {
    if let Some(observer) = observer() {
        observer.invalidate_generation();
    }
}
