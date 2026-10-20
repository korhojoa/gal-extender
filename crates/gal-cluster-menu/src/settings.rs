// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The settings the menu edits: each entry is a
//! `gal_loader.conf` key with a label, a type, a default and a range. Every
//! value is an int in the conf (the features read them with
//! `gal_feature_api::config::get_int`), BOOL and ENUM included.

use core::fmt::Write as _;

use crate::cbuf::CBuf;

/// `UiType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    Bool,
    Enum,
}

/// `UiEnumOpt`: one ENUM value and its label.
#[derive(Clone, Copy, Debug)]
pub struct EnumOpt {
    pub value: i32,
    pub label: &'static str,
}

/// `UiSetting`.
#[derive(Clone, Copy, Debug)]
pub struct Setting {
    /// The `gal_loader.conf` key.
    pub key: &'static str,
    /// The text the menu shows.
    pub label: &'static str,
    pub kind: Kind,
    /// The default when the key is not in the conf.
    pub def: i32,
    /// INT bounds and step.
    pub vmin: i32,
    pub vmax: i32,
    pub step: i32,
    /// ENUM options (no setting has any today).
    pub opts: &'static [EnumOpt],
    pub live: bool,
    /// 0 = the main list. 1 = the Cluster margins submenu.
    pub group: i32,
    /// The current value.
    pub value: i32,
    /// The value read from the conf (a setting is changed when they differ).
    pub file_value: i32,
    /// The key was in the conf, not commented out.
    pub present: bool,
}

/// One table row, runtime fields at the default.
#[allow(
    clippy::too_many_arguments,
    reason = "One argument per settings column"
)]
const fn row(
    key: &'static str,
    label: &'static str,
    kind: Kind,
    def: i32,
    vmin: i32,
    vmax: i32,
    step: i32,
    group: i32,
) -> Setting {
    Setting {
        key,
        label,
        kind,
        def,
        vmin,
        vmax,
        step,
        opts: &[],
        live: false,
        group,
        value: def,
        file_value: def,
        present: false,
    }
}

/// The number of settings.
pub const COUNT: usize = 10;

#[must_use]
pub fn table() -> [Setting; COUNT] {
    use Kind::{Bool, Int};
    [
        row(
            "cluster_density",
            "Cluster density",
            Int,
            180,
            60,
            320,
            1,
            0,
        ),
        row(
            "cluster_viewing_distance",
            "Viewing dist(mm)",
            Int,
            100,
            100,
            2000,
            10,
            0,
        ),
        row("cluster_focus_kick", "Focus recovery", Bool, 0, 0, 1, 1, 0),
        row(
            "sensor_ev_capacity_wh",
            "EV capacity(Wh)",
            Int,
            28500,
            0,
            120_000,
            500,
            0,
        ),
        row("cluster_margin_top", "Top", Int, 60, 0, 240, 2, 1),
        row("cluster_margin_bottom", "Bottom", Int, 96, 0, 240, 2, 1),
        row("cluster_margin_left", "Left", Int, 48, 0, 400, 2, 1),
        row("cluster_margin_right", "Right", Int, 48, 0, 400, 2, 1),
        row(
            "cluster_content_inset_left",
            "Inset left",
            Int,
            64,
            0,
            400,
            2,
            1,
        ),
        row(
            "cluster_content_inset_right",
            "Inset right",
            Int,
            64,
            0,
            400,
            2,
            1,
        ),
    ]
}

impl Setting {
    /// `ui_setting_format`: the value as the menu shows it ("180", "on",
    /// "off", an ENUM label).
    pub fn format<const N: usize>(&self, out: &mut CBuf<N>) {
        out.clear();
        match self.kind {
            Kind::Bool => out.push_bytes(if self.value != 0 { b"on" } else { b"off" }),
            Kind::Enum => {
                if let Some(o) = self.opts.iter().find(|o| o.value == self.value) {
                    out.push_bytes(o.label.as_bytes());
                } else {
                    let _ = write!(out, "{}", self.value);
                }
            }
            Kind::Int => {
                let _ = write!(out, "{}", self.value);
            }
        }
    }

    /// `ui_setting_conf_value`: the value as it goes into the conf.
    pub fn conf_value<const N: usize>(&self, out: &mut CBuf<N>) {
        out.set(format_args!("{}", self.value));
    }

    /// `ui_setting_adjust`: one step in `dir` (+1 / -1). INT clamps, BOOL
    /// toggles, ENUM wraps.
    pub fn adjust(&mut self, dir: i32) {
        match self.kind {
            Kind::Bool => self.value = i32::from(self.value == 0),
            Kind::Enum => {
                let Ok(n) = i32::try_from(self.opts.len()) else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let at = self
                    .opts
                    .iter()
                    .position(|o| o.value == self.value)
                    .and_then(|i| i32::try_from(i).ok())
                    .unwrap_or(0);
                let mut idx = (at + dir) % n;
                if idx < 0 {
                    idx += n;
                }
                if let Some(o) = usize::try_from(idx).ok().and_then(|i| self.opts.get(i)) {
                    self.value = o.value;
                }
            }
            Kind::Int => {
                let v = self.value.wrapping_add(dir.wrapping_mul(self.step));
                self.value = if v < self.vmin {
                    self.vmin
                } else if v > self.vmax {
                    self.vmax
                } else {
                    v
                };
            }
        }
    }

    /// `ui_setting_dirty`: the value differs from the conf's.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.value != self.file_value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_starts_at_the_defaults() {
        let t = table();
        assert_eq!(t.len(), 10);
        assert!(t
            .iter()
            .all(|s| s.value == s.def && s.file_value == s.def && !s.present));
        assert_eq!(t.iter().filter(|s| s.group == 1).count(), 6);
        assert!(t.iter().all(|s| !s.live && s.opts.is_empty()));
    }

    #[test]
    fn int_clamps_bool_toggles() {
        let mut t = table();
        let d = &mut t[1]; // viewing distance 100, 100..2000 step 10
        d.adjust(-1);
        assert_eq!(d.value, 100);
        d.adjust(1);
        assert_eq!(d.value, 110);
        assert!(d.dirty());
        let v = &mut t[2];
        v.adjust(-1);
        assert_eq!(v.value, 1);
        v.adjust(1);
        assert_eq!(v.value, 0);
        let mut b: CBuf<40> = CBuf::new();
        t[2].format(&mut b);
        assert_eq!(b.as_bytes(), b"off");
        t[3].format(&mut b);
        assert_eq!(b.as_bytes(), b"28500");
        t[3].conf_value(&mut b);
        assert_eq!(b.as_bytes(), b"28500");
    }

    #[test]
    fn enum_wraps_both_ways() {
        static OPTS: [EnumOpt; 3] = [
            EnumOpt {
                value: 5,
                label: "five",
            },
            EnumOpt {
                value: 7,
                label: "seven",
            },
            EnumOpt {
                value: 9,
                label: "nine",
            },
        ];
        let mut s = row("k", "l", Kind::Enum, 5, 0, 0, 1, 0);
        s.opts = &OPTS;
        s.adjust(-1);
        assert_eq!(s.value, 9);
        s.adjust(1);
        assert_eq!(s.value, 5);
        let mut b: CBuf<40> = CBuf::new();
        s.format(&mut b);
        assert_eq!(b.as_bytes(), b"five");
        s.value = 6;
        s.format(&mut b);
        assert_eq!(b.as_bytes(), b"6");
        s.adjust(1); // not found: from index 0
        assert_eq!(s.value, 7);
        s.opts = &[];
        s.adjust(1);
        assert_eq!(s.value, 7);
    }
}
