//! Raw evdev pen input: the full digitizer, bypassing Qt's filtered view.
//! Gives us 0-4095 pressure, tilt, hover, and the eraser tip (BTN_TOOL_RUBBER),
//! at the hardware event rate.
//!
//! The device is grabbed (EVIOCGRAB) while the diary is open so xochitl
//! doesn't also react to the pen; released automatically on close/exit.
//!
//! # reMarkable 2 port
//!
//! The Paper Pro's Elan digitizer was axis-aligned with the panel and its
//! maxima were hardcoded. The rM2's "Wacom I2C Digitizer" is NOT: the sensor
//! is mounted rotated 90 degrees, so its long axis (ABS_X, 0..20966, ~210 mm)
//! runs down the screen's HEIGHT and its short axis (ABS_Y, 0..15725,
//! ~157 mm) runs across the screen's WIDTH.
//!
//! Rather than hardcode another device's constants, we read the real ranges
//! with EVIOCGABS at open time and derive the swap from them: if the axis with
//! the larger range does not correspond to the larger screen dimension, the
//! axes are swapped. The two sign flips cannot be derived from the ranges, so
//! they default to the transform used by libremarkable for the rM2 Wacom
//! (rotation Rot270, no extra inversion), which works out to:
//!
//!   screen_x = raw_y * (W-1) / max_y
//!   screen_y = (max_x - raw_x) * (H-1) / max_x
//!
//! i.e. swap, do not invert X, DO invert Y. All three flags are overridable at
//! runtime (RIDDLE_PEN_SWAP / RIDDLE_PEN_INVERT_X / RIDDLE_PEN_INVERT_Y) so
//! the mapping can be corrected on the tablet without a rebuild.

use std::io;
use std::os::fd::RawFd;

use crate::fb::{SCREEN_H, SCREEN_W};

/// Fallback digitizer maxima if EVIOCGABS fails (rM2 Wacom, measured).
const FALLBACK_MAX_X: i32 = 20966;
const FALLBACK_MAX_Y: i32 = 15725;
/// rM2 Wacom reports ABS_PRESSURE 0..4095.
pub const MAX_PRESSURE: i32 = 4096;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const ABS_PRESSURE: u16 = 24;
const BTN_TOOL_PEN: u16 = 320;
const BTN_TOOL_RUBBER: u16 = 321;
const BTN_TOUCH: u16 = 330;

const EVIOCGRAB: libc::c_ulong = 0x40044590;

/// Size of the kernel's `struct input_event`. The leading `struct timeval`
/// uses `__kernel_ulong_t`, i.e. the architecture's `long`: 8 bytes on 32-bit
/// ARM (total 16) versus 16 on aarch64 (total 24). The Paper Pro build
/// hardcoded 24, which reads pure garbage on armv7 — this is the single most
/// important non-geometry fix in the port.
pub const EV_SIZE: usize = 2 * std::mem::size_of::<libc::c_long>() + 8;
/// Byte offset of `type` within `input_event` (right after the timeval).
pub const EV_TYPE_OFF: usize = 2 * std::mem::size_of::<libc::c_long>();

/// Decode one `input_event` record: (type, code, value).
#[inline]
pub fn decode_event(chunk: &[u8]) -> (u16, u16, i32) {
    let t = EV_TYPE_OFF;
    (
        u16::from_le_bytes(chunk[t..t + 2].try_into().unwrap()),
        u16::from_le_bytes(chunk[t + 2..t + 4].try_into().unwrap()),
        i32::from_le_bytes(chunk[t + 4..t + 8].try_into().unwrap()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Eraser,
}

#[derive(Debug, Clone, Copy)]
pub struct PenSample {
    /// Screen coordinates.
    pub x: i32,
    pub y: i32,
    /// 0..4095
    pub pressure: i32,
    pub tool: Tool,
    pub touching: bool,
    /// True from tool-in-range until the pen leaves the digitizer.
    pub proximity: bool,
}

/// How raw digitizer coordinates become screen pixels.
#[derive(Debug, Clone, Copy)]
struct Transform {
    /// Digitizer axis that feeds screen X, and its maximum.
    swap: bool,
    max_x: i32,
    max_y: i32,
    invert_x: bool,
    invert_y: bool,
}

impl Transform {
    #[inline]
    fn apply(&self, raw_x: i32, raw_y: i32) -> (i32, i32) {
        // Pick which raw axis drives which screen axis.
        let (a, a_max, b, b_max) = if self.swap {
            (raw_y, self.max_y, raw_x, self.max_x)
        } else {
            (raw_x, self.max_x, raw_y, self.max_y)
        };
        let mut sx = a.clamp(0, a_max) * (SCREEN_W as i32 - 1) / a_max.max(1);
        let mut sy = b.clamp(0, b_max) * (SCREEN_H as i32 - 1) / b_max.max(1);
        if self.invert_x {
            sx = SCREEN_W as i32 - 1 - sx;
        }
        if self.invert_y {
            sy = SCREEN_H as i32 - 1 - sy;
        }
        (sx, sy)
    }
}

pub struct PenDevice {
    fd: RawFd,
    tf: Transform,
    // Accumulated state between SYN_REPORTs.
    raw_x: i32,
    raw_y: i32,
    pressure: i32,
    tool: Tool,
    touching: bool,
    pen_in_range: bool,
    rubber_in_range: bool,
    proximity: bool,
    dirty: bool,
}

impl PenDevice {
    /// Find and grab the marker input device.
    pub fn open() -> io::Result<Self> {
        let path = find_marker_device()?;
        let cpath = std::ffi::CString::new(path.clone()).unwrap();
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let grab = unsafe { libc::ioctl(fd, EVIOCGRAB, 1i32) };
        if grab != 0 {
            eprintln!(
                "riddle: warning: EVIOCGRAB failed ({}) — xochitl will also see the pen",
                io::Error::last_os_error()
            );
        }
        let tf = derive_transform(fd);
        eprintln!(
            "riddle: pen device {path} opened (grabbed: {}), digitizer {}x{} -> {}x{} \
             swap={} invert_x={} invert_y={}",
            grab == 0,
            tf.max_x,
            tf.max_y,
            SCREEN_W,
            SCREEN_H,
            tf.swap,
            tf.invert_x,
            tf.invert_y
        );
        Ok(Self {
            fd,
            tf,
            raw_x: 0,
            raw_y: 0,
            pressure: 0,
            tool: Tool::Pen,
            touching: false,
            pen_in_range: false,
            rubber_in_range: false,
            proximity: false,
            dirty: false,
        })
    }

    /// The evdev descriptor, so the event loop can wait on it rather than poll.
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Drain all pending events; returns one sample per SYN_REPORT frame
    /// that changed state.
    pub fn drain(&mut self) -> Vec<PenSample> {
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
                match (etype, code) {
                    (EV_ABS, ABS_X) => {
                        self.raw_x = value;
                        self.dirty = true;
                    }
                    (EV_ABS, ABS_Y) => {
                        self.raw_y = value;
                        self.dirty = true;
                    }
                    (EV_ABS, ABS_PRESSURE) => {
                        self.pressure = value;
                        self.dirty = true;
                    }
                    (EV_KEY, BTN_TOOL_PEN) => {
                        self.pen_in_range = value == 1;
                        if self.pen_in_range {
                            self.tool = Tool::Pen;
                        }
                        self.proximity = self.pen_in_range || self.rubber_in_range;
                        self.dirty = true;
                    }
                    (EV_KEY, BTN_TOOL_RUBBER) => {
                        self.rubber_in_range = value == 1;
                        if self.rubber_in_range {
                            self.tool = Tool::Eraser;
                        }
                        self.proximity = self.pen_in_range || self.rubber_in_range;
                        self.dirty = true;
                    }
                    (EV_KEY, BTN_TOUCH) => {
                        self.touching = value == 1;
                        self.dirty = true;
                    }
                    (EV_SYN, SYN_REPORT) => {
                        if self.dirty {
                            self.dirty = false;
                            let (x, y) = self.tf.apply(self.raw_x, self.raw_y);
                            out.push(PenSample {
                                x,
                                y,
                                pressure: self.pressure,
                                tool: self.tool,
                                touching: self.touching,
                                proximity: self.proximity,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

impl Drop for PenDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, EVIOCGRAB, 0i32);
            libc::close(self.fd);
        }
    }
}

/// `struct input_absinfo` = 6 x __s32 { value, minimum, maximum, fuzz, flat,
/// resolution }. EVIOCGABS(a) = _IOR('E', 0x40 + a, 24).
fn eviocgabs(fd: RawFd, axis: u16) -> Option<i32> {
    let req: libc::c_ulong = 0x8018_4540 | (axis as libc::c_ulong);
    let mut info = [0i32; 6];
    let rc = unsafe { libc::ioctl(fd, req, info.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // maximum
    (info[2] > 0).then_some(info[2])
}

fn env_flag(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Read the real axis ranges and decide the raw -> screen mapping.
fn derive_transform(fd: RawFd) -> Transform {
    let max_x = eviocgabs(fd, ABS_X).unwrap_or(FALLBACK_MAX_X);
    let max_y = eviocgabs(fd, ABS_Y).unwrap_or(FALLBACK_MAX_Y);

    // The digitizer's long axis must line up with the panel's long axis. On
    // the rM2 that is ABS_X (20966) against a portrait 1404x1872 panel, so a
    // swap is required; on the Paper Pro's Elan it was not.
    let digitizer_is_landscape = max_x > max_y;
    let screen_is_landscape = SCREEN_W > SCREEN_H;
    let swap = env_flag("RIDDLE_PEN_SWAP").unwrap_or(digitizer_is_landscape != screen_is_landscape);

    // Signs cannot be inferred from the ranges. Defaults follow libremarkable's
    // rM2 Wacom placement (Rot270, invert_x = invert_y = false), which reduces
    // to "screen Y counts down from the digitizer's ABS_X maximum".
    let invert_x = env_flag("RIDDLE_PEN_INVERT_X").unwrap_or(false);
    let invert_y = env_flag("RIDDLE_PEN_INVERT_Y").unwrap_or(swap);

    Transform { swap, max_x, max_y, invert_x, invert_y }
}

fn find_marker_device() -> io::Result<String> {
    if let Ok(p) = std::env::var("RIDDLE_PEN_DEV") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    // rM2: event1 "Wacom I2C Digitizer". Paper Pro: "Elan marker input".
    for i in 0..32 {
        let name_path = format!("/sys/class/input/event{i}/device/name");
        if let Ok(name) = std::fs::read_to_string(&name_path) {
            let n = name.to_lowercase();
            if n.contains("marker") || n.contains("wacom") || n.contains("digitizer") {
                return Ok(format!("/dev/input/event{i}"));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no marker/wacom input device found",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rm2() -> Transform {
        Transform {
            swap: true,
            max_x: 20966,
            max_y: 15725,
            invert_x: false,
            invert_y: true,
        }
    }

    #[test]
    fn input_event_size_matches_pointer_width() {
        // 16 bytes on armv7 (32-bit long), 24 on aarch64/x86_64.
        assert_eq!(EV_SIZE, if cfg!(target_pointer_width = "32") { 16 } else { 24 });
        assert_eq!(EV_TYPE_OFF, EV_SIZE - 8);
    }

    #[test]
    fn wacom_corners_land_on_screen_corners() {
        let t = rm2();
        // Digitizer origin (0,0) -> bottom-left of the portrait page.
        assert_eq!(t.apply(0, 0), (0, SCREEN_H as i32 - 1));
        // ABS_X max -> top of the page; ABS_Y max -> right edge.
        assert_eq!(t.apply(20966, 0), (0, 0));
        assert_eq!(t.apply(0, 15725), (SCREEN_W as i32 - 1, SCREEN_H as i32 - 1));
        assert_eq!(t.apply(20966, 15725), (SCREEN_W as i32 - 1, 0));
    }

    #[test]
    fn wacom_centre_is_screen_centre() {
        let t = rm2();
        let (x, y) = t.apply(20966 / 2, 15725 / 2);
        assert!((x - SCREEN_W as i32 / 2).abs() <= 2, "x={x}");
        assert!((y - SCREEN_H as i32 / 2).abs() <= 2, "y={y}");
    }

    #[test]
    fn out_of_range_input_is_clamped_inside_the_page() {
        let t = rm2();
        let (x, y) = t.apply(99_999, -50);
        assert!((0..SCREEN_W as i32).contains(&x));
        assert!((0..SCREEN_H as i32).contains(&y));
    }

    #[test]
    fn unswapped_axis_aligned_digitizer_is_identity_like() {
        // Paper-Pro-style (portrait digitizer, portrait screen): no swap, no flips.
        let t = Transform { swap: false, max_x: 11180, max_y: 15340, invert_x: false, invert_y: false };
        assert_eq!(t.apply(0, 0), (0, 0));
        assert_eq!(t.apply(11180, 15340), (SCREEN_W as i32 - 1, SCREEN_H as i32 - 1));
    }
}
