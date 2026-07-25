//! Raw multitouch gestures for takeover mode.
//! One-finger swipe pages, two-finger drag scrolls, two-finger tap undoes,
//! three-finger tap redoes, and five fingers exits.
//!
//! reMarkable 2 note: the "pt_mt" panel reports ABS_MT_POSITION_X 0..1403 and
//! ABS_MT_POSITION_Y 0..1871 — the same numbers as the framebuffer, but the
//! panel's origin is at the BOTTOM of the page, so Y must be inverted
//! (libremarkable models this as Rot180 + invert_x for the rM2 multitouch,
//! which cancels on X and flips only Y). Only the Y axis is used here.
//!
//! NOTE: this module is only reachable in takeover mode, which is dead on the
//! rM2 (no libquill). It is kept correct rather than deleted.

use std::io;
use std::os::fd::RawFd;

use crate::fb::SCREEN_H;
use crate::pen::{decode_event, EV_SIZE};

const EV_SYN: u16 = 0;
const SYN_REPORT: u16 = 0;
const EV_ABS: u16 = 3;
const ABS_MT_SLOT: u16 = 47;
const ABS_MT_POSITION_Y: u16 = 54;
const ABS_MT_TRACKING_ID: u16 = 57;
const EVIOCGRAB: libc::c_ulong = 0x40044590;
const MAX_SLOTS: usize = 16;
/// rM2 pt_mt maximum for ABS_MT_POSITION_Y (fallback; read via EVIOCGABS).
const FALLBACK_TOUCH_MAX_Y: i32 = 1871;
const TAP_SLOP: i32 = 45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Quit,
    Undo,
    Redo,
    /// Positive values move down through the document.
    Scroll(i32),
    /// Direction (+1 down, -1 up); caller chooses page size.
    Page(i32),
}

#[derive(Clone, Copy, Default)]
struct Slot {
    active: bool,
    start_y: i32,
    y: i32,
}

pub struct TouchDevice {
    fd: RawFd,
    slots: [Slot; MAX_SLOTS],
    cur: usize,
    max_fingers: usize,
    frame_y: Option<i32>,
    total_motion: i32,
    quit_sent: bool,
    /// ABS_MT_POSITION_Y maximum, read from the device.
    max_y: i32,
}

impl TouchDevice {
    pub fn open() -> io::Result<Self> {
        // rM2: event2 "pt_mt". Paper Pro / rM1: names containing "touch".
        for i in 0..32 {
            let name_path = format!("/sys/class/input/event{i}/device/name");
            let Ok(name) = std::fs::read_to_string(&name_path) else { continue };
            let n = name.to_lowercase();
            if !(n.contains("touch") || n.contains("pt_mt") || n.contains("multi")) {
                continue;
            }
            let path = std::ffi::CString::new(format!("/dev/input/event{i}")).unwrap();
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe { libc::ioctl(fd, EVIOCGRAB, 1i32) };
            let max_y = abs_max(fd, ABS_MT_POSITION_Y).unwrap_or(FALLBACK_TOUCH_MAX_Y);
            return Ok(Self {
                fd,
                slots: [Slot::default(); MAX_SLOTS],
                cur: 0,
                max_fingers: 0,
                frame_y: None,
                total_motion: 0,
                quit_sent: false,
                max_y,
            });
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "no touch device"))
    }

    /// Panel Y (origin at the bottom of the page) -> screen Y.
    #[inline]
    fn screen_y(&self, raw: i32) -> i32 {
        let clamped = raw.clamp(0, self.max_y);
        (self.max_y - clamped) * (SCREEN_H as i32 - 1) / self.max_y.max(1)
    }

    /// Drain and discard touch input, then cancel every partial gesture. Used
    /// for palm rejection while the marker is in digitizer proximity.
    pub fn suppress(&mut self) {
        let _ = self.drain();
        self.slots = [Slot::default(); MAX_SLOTS];
        self.max_fingers = 0;
        self.frame_y = None;
        self.total_motion = 0;
        self.quit_sent = false;
    }

    /// Compatibility helper for takeover apps that only use five-finger exit.
    pub fn drain_check_quit(&mut self) -> bool {
        self.drain().contains(&Gesture::Quit)
    }

    pub fn drain(&mut self) -> Vec<Gesture> {
        let mut out = Vec::new();
        let mut buf = [0u8; EV_SIZE * 64];
        loop {
            let n =
                unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            for chunk in buf[..n as usize].chunks_exact(EV_SIZE) {
                let (etype, code, value) = decode_event(chunk);
                if etype == EV_ABS && code == ABS_MT_SLOT {
                    self.cur = (value.max(0) as usize).min(MAX_SLOTS - 1);
                } else if etype == EV_ABS && code == ABS_MT_POSITION_Y {
                    let value = self.screen_y(value);
                    self.slots[self.cur].y = value;
                    if self.slots[self.cur].active && self.slots[self.cur].start_y == i32::MIN {
                        self.slots[self.cur].start_y = value;
                    }
                } else if etype == EV_ABS && code == ABS_MT_TRACKING_ID {
                    if value != -1 {
                        self.slots[self.cur] = Slot {
                            active: true,
                            start_y: i32::MIN,
                            y: self.slots[self.cur].y,
                        };
                    } else {
                        self.slots[self.cur].active = false;
                    }
                } else if etype == EV_SYN && code == SYN_REPORT {
                    self.finish_frame(&mut out);
                }
            }
        }
        out
    }

    fn finish_frame(&mut self, out: &mut Vec<Gesture>) {
        let active: Vec<Slot> = self.slots.iter().copied().filter(|s| s.active).collect();
        let count = active.len();
        self.max_fingers = self.max_fingers.max(count);
        if count >= 5 && !self.quit_sent {
            self.quit_sent = true;
            out.push(Gesture::Quit);
        }

        let average_y = (count > 0).then(|| active.iter().map(|s| s.y).sum::<i32>() / count as i32);
        if let (Some(previous), Some(current)) = (self.frame_y, average_y) {
            let raw_delta = previous - current;
            self.total_motion += raw_delta.abs();
            if count == 2 {
                // Slot Y is already in screen pixels (see `screen_y`).
                if raw_delta != 0 {
                    out.push(Gesture::Scroll(raw_delta));
                }
            }
        }
        self.frame_y = average_y;

        if count == 0 && self.max_fingers > 0 {
            if self.total_motion < TAP_SLOP {
                match self.max_fingers {
                    2 => out.push(Gesture::Undo),
                    3 => out.push(Gesture::Redo),
                    _ => {}
                }
            } else if self.max_fingers == 1 {
                // Released slots retain their coordinates.
                if let Some(slot) = self
                    .slots
                    .iter()
                    .max_by_key(|slot| (slot.start_y - slot.y).abs())
                {
                    let delta = slot.start_y - slot.y;
                    if delta.abs() >= TAP_SLOP {
                        out.push(Gesture::Page(delta.signum()));
                    }
                }
            }
            self.max_fingers = 0;
            self.frame_y = None;
            self.total_motion = 0;
            self.quit_sent = false;
        }
    }
}

/// EVIOCGABS(axis) -> `maximum`. See `pen::eviocgabs` for the encoding.
fn abs_max(fd: RawFd, axis: u16) -> Option<i32> {
    let req: libc::c_ulong = 0x8018_4540 | (axis as libc::c_ulong);
    let mut info = [0i32; 6];
    if unsafe { libc::ioctl(fd, req, info.as_mut_ptr()) } != 0 {
        return None;
    }
    (info[2] > 0).then_some(info[2])
}

impl Drop for TouchDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, EVIOCGRAB, 0i32);
            libc::close(self.fd);
        }
    }
}
