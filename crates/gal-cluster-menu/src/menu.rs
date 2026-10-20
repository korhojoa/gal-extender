// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The menu: its state machine and its drawing.
//!
//! View 0 is the main list: its settings, then "Cluster margins >", "[ Save &
//! exit ]" and "[ Exit ]". View 1 is the margins submenu: its settings, then
//! "< Back". Input is an abstract event (`EV_*`). the feature maps the
//! steering-wheel keys onto it.

use core::ffi::CStr;
use core::fmt::Write as _;

use crate::cbuf::CBuf;
use crate::conf;
use crate::fb::{self, Fb};
use crate::settings::{Setting, COUNT};

/// Browse: up. Edit: the value down.
pub const EV_PREV: i32 = 0;
/// Browse: down. Edit: the value up.
pub const EV_NEXT: i32 = 1;
/// Browse: enter edit (or open a row). Edit: confirm.
pub const EV_SELECT: i32 = 2;
/// Edit: leave edit. Browse: back to view 0, else close.
pub const EV_BACK: i32 = 3;
/// Write the changed settings to the conf.
pub const EV_SAVE: i32 = 4;

/// Layout (pixels). Rows scroll when they do not all fit.
const TITLE_H: i32 = 22;
const ROW_H: i32 = 18;
const FOOT_H: i32 = 16;

/// What a row of the current view is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// A setting (its index in the table).
    Setting(usize),
    /// "Cluster margins >" (the view it opens).
    Submenu(i32),
    Back,
    Save,
    Exit,
}

/// `UiMenu`.
pub struct Menu {
    /// The settings (loaded from the conf by the caller).
    pub items: [Setting; COUNT],
    /// The selected row of the current view.
    pub sel: i32,
    /// 0 = the main list, 1 = the Cluster margins submenu.
    pub view: i32,
    /// The first visible row (scroll).
    pub first: i32,
    /// Editing the selected setting.
    pub editing: bool,
    /// The caller should close the menu.
    pub want_close: bool,
    /// The caller should save and close.
    pub want_save: bool,
    /// The status line (`char status[80]`).
    pub status: CBuf<80>,
    /// The content box inside the framebuffer: the visible cluster window
    /// (the rest of the frame is behind the gauges). Size 0: the whole frame.
    pub cx: i32,
    pub cy: i32,
    pub cw: i32,
    pub ch: i32,
}

impl Menu {
    /// `ui_menu_init`.
    #[must_use]
    pub fn new(items: [Setting; COUNT]) -> Menu {
        Menu {
            items,
            sel: 0,
            view: 0,
            first: 0,
            editing: false,
            want_close: false,
            want_save: false,
            status: CBuf::new(),
            cx: 0,
            cy: 0,
            cw: 0,
            ch: 0,
        }
    }

    /// `rows_of_view`: the number of rows of the current view, and row
    /// `want` (when it exists).
    fn rows(&self, want: i32) -> (i32, Option<Row>) {
        let mut idx = 0i32;
        let mut found = None;
        let mut take = |idx: &mut i32, r: Row| {
            if *idx == want {
                found = Some(r);
            }
            *idx += 1;
        };
        for (j, s) in self.items.iter().enumerate() {
            if s.group == self.view {
                take(&mut idx, Row::Setting(j));
            }
        }
        if self.view == 0 {
            take(&mut idx, Row::Submenu(1));
            take(&mut idx, Row::Save);
            take(&mut idx, Row::Exit);
        } else {
            take(&mut idx, Row::Back);
        }
        (idx, found)
    }

    /// `ui_menu_dirty`: any setting changed.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.items.iter().any(Setting::dirty)
    }

    /// `enter_view`.
    fn enter_view(&mut self, v: i32) {
        self.view = v;
        self.sel = 0;
        self.first = 0;
        self.editing = false;
    }

    /// `ui_menu_input`: one event. `save_path` is used by `EV_SAVE` (None:
    /// saving is off). Returns 1 when the menu changed.
    pub fn input(&mut self, ev: i32, save_path: Option<&CStr>) -> i32 {
        self.status.clear();
        let (total, _) = self.rows(-1);
        if self.sel >= total {
            self.sel = total - 1;
        }
        if self.sel < 0 {
            self.sel = 0;
        }
        let kind = self.rows(self.sel).1.unwrap_or(Row::Exit);
        match ev {
            EV_PREV => {
                match kind {
                    Row::Setting(i) if self.editing => self.items[i].adjust(-1),
                    _ => self.sel = (self.sel - 1 + total) % total,
                }
                1
            }
            EV_NEXT => {
                match kind {
                    Row::Setting(i) if self.editing => self.items[i].adjust(1),
                    _ => self.sel = (self.sel + 1) % total,
                }
                1
            }
            EV_SELECT => {
                match kind {
                    Row::Setting(_) => self.editing = !self.editing,
                    Row::Submenu(v) => self.enter_view(v),
                    Row::Back => self.enter_view(0),
                    Row::Save => {
                        self.editing = false;
                        self.want_save = true;
                    }
                    Row::Exit => {
                        self.editing = false;
                        self.want_close = true;
                    }
                }
                1
            }
            EV_BACK => {
                if self.editing {
                    self.editing = false;
                } else if self.view != 0 {
                    self.enter_view(0);
                } else {
                    self.want_close = true;
                }
                1
            }
            EV_SAVE => {
                self.editing = false;
                if !self.dirty() {
                    self.status.set(format_args!("no changes"));
                    return 1;
                }
                let Some(path) = save_path else {
                    self.status.set(format_args!("save disabled"));
                    return 1;
                };
                let mut err: CBuf<96> = CBuf::new();
                let r = conf::save(path.to_bytes(), &mut self.items, &mut err);
                self.status.clear();
                if r < 0 {
                    let _ = self.status.write_str("SAVE FAILED: ");
                    self.status.push_bytes(err.as_bytes());
                } else {
                    let _ = write!(self.status, "saved {r} setting(s) - reconnect to apply");
                }
                1
            }
            _ => 0,
        }
    }

    /// `ui_menu_render`: the whole frame. Everything outside the content box
    /// is black. the menu is drawn inside it, scrolled so the selection is
    /// visible.
    pub fn render(&mut self, fb: &mut Fb<'_>) {
        fb.clear((0, 0, 0));
        let (mut bx, mut by, mut bw, mut bh) = (self.cx, self.cy, self.cw, self.ch);
        if bw <= 0 || bh <= 0 {
            (bx, by, bw, bh) = (0, 0, fb.w, fb.h);
        }
        fb.rect(bx, by, bw, bh, (12, 14, 20)); // panel background
        fb.rect(bx, by, bw, TITLE_H, (30, 60, 110)); // title bar
        let title: &[u8] = if self.view == 1 {
            b"Cluster margins"
        } else {
            b"gal-extender settings"
        };
        fb.text(bx + 8, by + 3, title, 1, (235, 240, 255));
        if self.dirty() {
            fb.text(bx + bw - 16, by + 3, b"*", 1, (255, 210, 120));
        }

        let top = by + TITLE_H + 4;
        let foot_y = by + bh - FOOT_H;
        let (total, _) = self.rows(-1);
        let mut visible = (foot_y - top) / ROW_H;
        if visible < 1 {
            visible = 1;
        }
        if self.sel < self.first {
            self.first = self.sel;
        }
        if self.sel >= self.first + visible {
            self.first = self.sel - visible + 1;
        }
        if self.first < 0 {
            self.first = 0;
        }

        let mut i = 0;
        while i < visible && self.first + i < total {
            let idx = self.first + i;
            let y = top + i * ROW_H;
            let is_sel = idx == self.sel;
            let kind = self.rows(idx).1.unwrap_or(Row::Exit);
            if is_sel {
                let c = if self.editing {
                    (150, 110, 40)
                } else {
                    (40, 90, 150)
                };
                fb.rect(bx + 4, y - 1, bw - 8, ROW_H, c);
            }
            let label_c = if is_sel {
                (255, 255, 255)
            } else {
                (200, 210, 225)
            };
            if let Row::Setting(j) = kind {
                let s = &self.items[j];
                fb.text(bx + 10, y, s.label.as_bytes(), 1, label_c);
                let mut val: CBuf<40> = CBuf::new();
                s.format(&mut val);
                let mut shown: CBuf<48> = CBuf::new();
                if is_sel && self.editing {
                    let _ = shown.write_str("< ");
                    shown.push_bytes(val.as_bytes());
                    let _ = shown.write_str(" >");
                } else {
                    shown.push_bytes(val.as_bytes());
                }
                let vw = fb::text_w(shown.as_bytes(), 1);
                let vc = if s.dirty() {
                    (255, 220, 120) // changed
                } else {
                    (170, 230, 170)
                };
                fb.text(bx + bw - vw - 10, y, shown.as_bytes(), 1, vc);
            } else {
                let c = if is_sel {
                    (255, 235, 160)
                } else {
                    (190, 205, 150)
                };
                fb.text(bx + 10, y, nav_label(kind), 1, c);
            }
            i += 1;
        }

        // Scroll marks.
        if self.first > 0 {
            fb.text(bx + bw - 12, top, b"^", 1, (150, 160, 175));
        }
        if self.first + visible < total {
            fb.text(bx + bw - 12, foot_y - ROW_H, b"v", 1, (150, 160, 175));
        }
        // Footer: the status when there is one, else the key legend.
        if self.status.is_empty() {
            fb.text(
                bx + 8,
                foot_y,
                b"next/prev: move   hold voice: select",
                1,
                (150, 160, 175),
            );
        } else {
            let st = self.status;
            fb.text(bx + 8, foot_y, st.as_bytes(), 1, (250, 230, 150));
        }
    }
}

/// `nav_label`.
fn nav_label(kind: Row) -> &'static [u8] {
    match kind {
        Row::Submenu(_) => b"Cluster margins   >",
        Row::Back => b"< Back",
        Row::Save => b"[ Save & exit ]",
        Row::Exit => b"[ Exit ]",
        Row::Setting(_) => b"?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::table;

    #[test]
    fn rows_of_both_views() {
        let mut m = Menu::new(table());
        assert_eq!(m.rows(-1).0, 7);
        assert_eq!(m.rows(4).1, Some(Row::Submenu(1)));
        assert_eq!(m.rows(6).1, Some(Row::Exit));
        m.view = 1;
        assert_eq!(m.rows(-1).0, 7);
        assert_eq!(m.rows(0).1, Some(Row::Setting(4)));
        assert_eq!(m.rows(6).1, Some(Row::Back));
        assert_eq!(m.rows(7).1, None);
    }

    #[test]
    fn browse_edit_and_actions() {
        let mut m = Menu::new(table());
        assert_eq!(m.input(EV_PREV, None), 1);
        assert_eq!(m.sel, 6); // wraps to [ Exit ]
        m.input(EV_NEXT, None);
        assert_eq!(m.sel, 0);
        m.input(EV_SELECT, None);
        assert!(m.editing);
        m.input(EV_NEXT, None);
        assert_eq!(m.items[0].value, 181);
        m.input(EV_PREV, None);
        m.input(EV_PREV, None);
        assert_eq!(m.items[0].value, 179);
        m.input(EV_BACK, None);
        assert!(!m.editing);
        m.sel = 4;
        m.input(EV_SELECT, None);
        assert_eq!((m.view, m.sel), (1, 0));
        m.input(EV_BACK, None);
        assert_eq!(m.view, 0);
        m.input(EV_BACK, None);
        assert!(m.want_close);
        m.sel = 5;
        m.input(EV_SELECT, None);
        assert!(m.want_save);
        m.sel = 99; // clamped to the last row
        m.input(EV_NEXT, None);
        assert_eq!(m.sel, 0);
        assert_eq!(m.input(99, None), 0);
        m.input(EV_SAVE, None);
        assert_eq!(m.status.as_bytes(), b"save disabled");
        m.items[0].value = m.items[0].file_value;
        m.input(EV_SAVE, None);
        assert_eq!(m.status.as_bytes(), b"no changes");
        m.input(EV_NEXT, None);
        assert!(m.status.is_empty());
    }
}
