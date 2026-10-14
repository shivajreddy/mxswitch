//! Minimal HID++ 2.0 client for Logitech's ChangeHost (Easy-Switch) feature.
//!
//! Long report (20 bytes):
//!   byte 0   report id      0x11
//!   byte 1   device index   0xFF direct (Bluetooth/USB), 1..6 behind a receiver
//!   byte 2   feature index  position of the feature in this device's table (resolved at runtime)
//!   byte 3   (function << 4) | software id
//!   byte 4+  parameters
//!
//! A successful setCurrentHost never replies: by then the device has already left this host.

use hidapi::{BusType, DeviceInfo, HidApi, HidDevice, HidError};
use std::ffi::CString;
use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub const LOGITECH_VID: u16 = 0x046D;

const REPORT_SHORT: u8 = 0x10;
const REPORT_LONG: u8 = 0x11;
const LONG_LEN: usize = 20;
const SW_ID: u8 = 0x0B;

const ROOT_IDX: u8 = 0x00;
const FEATURE_DEVICE_NAME: u16 = 0x0005;
const FEATURE_CHANGE_HOST: u16 = 0x1814;
const FEATURE_HOSTS_INFO: u16 = 0x1815;
const FEATURE_REPROG_CONTROLS_V4: u16 = 0x1B04;

/// Control IDs (buttons) as used by feature 0x1B04.
pub const CID_MIDDLE: u16 = 0x0052;
pub const CID_BACK: u16 = 0x0053;
pub const CID_FORWARD: u16 = 0x0056;
pub const CID_GESTURE: u16 = 0x00C3;
pub const CID_SMART_SHIFT: u16 = 0x00C4;

const CONTROL_DIVERTABLE: u8 = 0x20;
const DIVERT_ON: u8 = 0x03; // divert + divert-valid
const DIVERT_OFF: u8 = 0x02; // divert-valid only

const ERR_HIDPP20: u8 = 0xFF;
const ERR_HIDPP10: u8 = 0x8F;

const DEVICE_TYPE_KEYBOARD: u8 = 0;
const DEVICE_TYPE_MOUSE: u8 = 3;

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const REPLY_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug)]
pub enum Error {
    Hid(HidError),
    Timeout,
    DeviceError(u8),
    NotFound(String),
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "HID error: {e}"),
            Error::Timeout => write!(f, "no reply from the device"),
            Error::DeviceError(code) => write!(f, "the device returned HID++ error {code}"),
            Error::NotFound(msg) | Error::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<HidError> for Error {
    fn from(e: HidError) -> Self {
        Error::Hid(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A device that answered a ChangeHost probe.
#[derive(Clone, Debug)]
pub struct Target {
    pub path: CString,
    pub dev_idx: u8,
    pub feat_idx: u8,
    pub name: String,
    pub device_type: Option<u8>,
    pub transport: String,
}

impl Target {
    pub fn is_mouse(&self) -> bool {
        if self.device_type == Some(DEVICE_TYPE_MOUSE) {
            return true;
        }
        let n = self.name.to_ascii_lowercase();
        ["master", "anywhere", "mouse", "ergo", "lift", "vertical", "pebble", "signature"]
            .iter()
            .any(|k| n.contains(k))
    }

    pub fn is_keyboard(&self) -> bool {
        if self.device_type == Some(DEVICE_TYPE_KEYBOARD) {
            return true;
        }
        let n = self.name.to_ascii_lowercase();
        ["keys", "keyboard", "craft"].iter().any(|k| n.contains(k))
    }

    fn same_as(&self, other: &Target) -> bool {
        self.path == other.path && self.dev_idx == other.dev_idx
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HostInfo {
    pub count: u8,
    /// Zero-based: 0 = channel 1.
    pub current: u8,
}

#[derive(Clone, Debug)]
pub struct HostSlot {
    pub index: u8,
    pub paired: bool,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Switched { name: String, from: u8, to: u8 },
    AlreadyThere { name: String, host: u8 },
}

/// A reprogrammable control (button) from feature 0x1B04.
#[derive(Clone, Debug)]
pub struct Control {
    pub cid: u16,
    pub flags: u8,
}

impl Control {
    pub fn divertable(&self) -> bool {
        self.flags & CONTROL_DIVERTABLE != 0
    }
}

pub fn control_name(cid: u16) -> &'static str {
    match cid {
        0x0050 => "Left button",
        0x0051 => "Right button",
        CID_MIDDLE => "Middle button",
        CID_BACK => "Back button",
        CID_FORWARD => "Forward button",
        CID_GESTURE => "Gesture button",
        CID_SMART_SHIFT => "Smart Shift button",
        0x00D7 => "Virtual gesture button",
        _ => "",
    }
}

/// Parses `gesture`, `back`, `forward`, `middle`, `smartshift`, or a hex/decimal control ID.
pub fn parse_button(s: &str) -> Option<u16> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "gesture" | "thumb" => Some(CID_GESTURE),
        "back" => Some(CID_BACK),
        "forward" => Some(CID_FORWARD),
        "middle" => Some(CID_MIDDLE),
        "smartshift" | "smart-shift" | "modeshift" | "mode" => Some(CID_SMART_SHIFT),
        _ => match t.strip_prefix("0x") {
            Some(hex) => u16::from_str_radix(hex, 16).ok(),
            None => t.parse::<u16>().ok(),
        },
    }
}

fn request(
    dev: &HidDevice,
    dev_idx: u8,
    feat_idx: u8,
    func: u8,
    params: &[u8],
    timeout: Duration,
) -> Result<[u8; 16]> {
    let header = (func << 4) | SW_ID;
    let mut frame = [0u8; LONG_LEN];
    frame[0] = REPORT_LONG;
    frame[1] = dev_idx;
    frame[2] = feat_idx;
    frame[3] = header;
    frame[4..4 + params.len()].copy_from_slice(params);

    let mut buf = [0u8; 64];
    // Drop stale notifications so they are not mistaken for the reply.
    while matches!(dev.read_timeout(&mut buf, 0), Ok(n) if n > 0) {}

    dev.write(&frame)?;

    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::Timeout);
        }
        let n = dev.read_timeout(&mut buf, left.as_millis().max(1) as i32)?;
        if n < 4 {
            continue;
        }
        let r = &buf[..n];
        if r[0] != REPORT_SHORT && r[0] != REPORT_LONG {
            continue;
        }
        // Some Bluetooth devices answer a 0xFF request with device index 0.
        if r[1] != dev_idx && !(dev_idx == 0xFF && r[1] == 0x00) {
            continue;
        }
        if r[2] == ERR_HIDPP20 || r[2] == ERR_HIDPP10 {
            if n >= 6 && r[3] == feat_idx && r[4] == header {
                return Err(Error::DeviceError(r[5]));
            }
            continue;
        }
        if r[2] == feat_idx && r[3] == header {
            let mut out = [0u8; 16];
            let m = (n - 4).min(16);
            out[..m].copy_from_slice(&r[4..4 + m]);
            return Ok(out);
        }
    }
}

/// IRoot.getFeature: where a feature sits in this device's table (0 = unsupported).
fn feature_index(dev: &HidDevice, dev_idx: u8, feature: u16, timeout: Duration) -> Result<u8> {
    let params = [(feature >> 8) as u8, feature as u8, 0];
    Ok(request(dev, dev_idx, ROOT_IDX, 0, &params, timeout)?[0])
}

pub fn host_info(dev: &HidDevice, t: &Target, timeout: Duration) -> Result<HostInfo> {
    let r = request(dev, t.dev_idx, t.feat_idx, 0, &[], timeout)?;
    Ok(HostInfo { count: r[0], current: r[1] })
}

/// Feature index of ReprogControlsV4 on this device (0 if unsupported).
pub fn reprog_feature(dev: &HidDevice, t: &Target) -> Result<u8> {
    feature_index(dev, t.dev_idx, FEATURE_REPROG_CONTROLS_V4, REPLY_TIMEOUT)
}

pub fn controls(dev: &HidDevice, t: &Target) -> Result<Vec<Control>> {
    let feat = reprog_feature(dev, t)?;
    if feat == 0 {
        return Ok(Vec::new());
    }
    let count = request(dev, t.dev_idx, feat, 0, &[], REPLY_TIMEOUT)?[0];
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let r = request(dev, t.dev_idx, feat, 1, &[i], REPLY_TIMEOUT)?;
        out.push(Control { cid: u16::from_be_bytes([r[0], r[1]]), flags: r[4] });
    }
    Ok(out)
}

/// Diverted buttons stop doing their default action and are reported to us instead.
/// The setting is volatile: the device forgets it whenever it reconnects.
pub fn set_divert(dev: &HidDevice, t: &Target, feat: u8, cid: u16, on: bool) -> Result<()> {
    let flags = if on { DIVERT_ON } else { DIVERT_OFF };
    request(dev, t.dev_idx, feat, 3, &[(cid >> 8) as u8, cid as u8, flags, 0, 0], REPLY_TIMEOUT)?;
    Ok(())
}

pub fn is_diverted(dev: &HidDevice, t: &Target, feat: u8, cid: u16) -> Result<bool> {
    let r = request(dev, t.dev_idx, feat, 2, &[(cid >> 8) as u8, cid as u8], REPLY_TIMEOUT)?;
    Ok(r[2] & 0x01 != 0)
}

/// If `report` is a divertedButtonsEvent from this device, returns the control IDs currently held.
pub fn diverted_buttons_event(report: &[u8], t: &Target, feat: u8) -> Option<Vec<u16>> {
    if report.len() < 12 || report[0] != REPORT_LONG {
        return None;
    }
    if report[1] != t.dev_idx && !(t.dev_idx == 0xFF && report[1] == 0x00) {
        return None;
    }
    // Event 0 of the feature; notifications carry software id 0, replies carry ours.
    if report[2] != feat || report[3] != 0x00 {
        return None;
    }
    Some(
        (0..4)
            .map(|i| u16::from_be_bytes([report[4 + 2 * i], report[5 + 2 * i]]))
            .filter(|&cid| cid != 0)
            .collect(),
    )
}

/// DeviceTypeAndName: the marketing name and the device type.
fn device_name(dev: &HidDevice, dev_idx: u8) -> Result<(String, Option<u8>)> {
    let feat = feature_index(dev, dev_idx, FEATURE_DEVICE_NAME, REPLY_TIMEOUT)?;
    if feat == 0 {
        return Ok((String::new(), None));
    }
    let len = request(dev, dev_idx, feat, 0, &[], REPLY_TIMEOUT)?[0] as usize;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let chunk = request(dev, dev_idx, feat, 1, &[bytes.len() as u8], REPLY_TIMEOUT)?;
        let take = (len - bytes.len()).min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
    }
    let name = String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string();
    let kind = request(dev, dev_idx, feat, 2, &[], REPLY_TIMEOUT).ok().map(|r| r[0]);
    Ok((name, kind))
}

/// HostsInfo: which channels are paired and the names of the paired hosts (best effort).
fn host_slots(dev: &HidDevice, t: &Target) -> Result<Vec<HostSlot>> {
    let feat = feature_index(dev, t.dev_idx, FEATURE_HOSTS_INFO, REPLY_TIMEOUT)?;
    if feat == 0 {
        return Ok(Vec::new());
    }
    let r = request(dev, t.dev_idx, feat, 0, &[], REPLY_TIMEOUT)?;
    let has_names = u16::from_be_bytes([r[0], r[1]]) & 0x01 != 0;
    let count = r[2];
    let mut slots = Vec::new();
    for h in 0..count {
        let info = request(dev, t.dev_idx, feat, 1, &[h], REPLY_TIMEOUT)?;
        let paired = info[1] != 0;
        let name_len = info[4] as usize;
        let mut name = String::new();
        if has_names && paired && name_len > 0 {
            let mut bytes = Vec::with_capacity(name_len);
            while bytes.len() < name_len {
                let piece = request(dev, t.dev_idx, feat, 3, &[h, bytes.len() as u8], REPLY_TIMEOUT)?;
                let take = (name_len - bytes.len()).min(piece.len() - 2);
                bytes.extend_from_slice(&piece[2..2 + take]);
            }
            name = String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string();
        }
        slots.push(HostSlot { index: h, paired, name });
    }
    Ok(slots)
}

fn bus_name(bus: BusType) -> &'static str {
    match bus {
        BusType::Bluetooth => "Bluetooth",
        BusType::Usb => "USB",
        _ => "direct",
    }
}

fn transport(info: &DeviceInfo, dev_idx: u8) -> String {
    if dev_idx != 0xFF {
        format!("receiver, slot {dev_idx}")
    } else {
        bus_name(info.bus_type()).to_string()
    }
}

/// Sends setCurrentHost and confirms by absence: the device stops answering once it has left.
pub fn switch(dev: &HidDevice, t: &Target, info: HostInfo, host: u8) -> Result<Outcome> {
    if info.current == host {
        return Ok(Outcome::AlreadyThere { name: t.name.clone(), host });
    }
    if host >= info.count {
        return Err(Error::Failed(format!(
            "channel {} does not exist – {} has {} channels",
            host + 1,
            t.name,
            info.count
        )));
    }
    let mut frame = [0u8; LONG_LEN];
    frame[0] = REPORT_LONG;
    frame[1] = t.dev_idx;
    frame[2] = t.feat_idx;
    frame[3] = (1 << 4) | SW_ID;
    frame[4] = host;

    for attempt in 0..4 {
        if attempt > 0 {
            sleep(Duration::from_millis(200));
        }
        dev.write(&frame)?;
        sleep(Duration::from_millis(250));
        match host_info(dev, t, Duration::from_millis(300)) {
            Ok(now) if now.current != host => continue,
            _ => {
                return Ok(Outcome::Switched { name: t.name.clone(), from: info.current, to: host })
            }
        }
    }
    Err(Error::Failed(format!(
        "{} did not leave this PC – is channel {} paired and that PC awake?",
        t.name,
        host + 1
    )))
}

pub struct Switcher {
    api: HidApi,
    filter: Option<String>,
    cached: Option<Target>,
}

impl Switcher {
    pub fn new(filter: Option<String>) -> Result<Self> {
        Ok(Switcher { api: HidApi::new()?, filter, cached: None })
    }

    /// Every Logitech device on this PC that supports ChangeHost.
    pub fn discover(&mut self) -> Result<Vec<Target>> {
        self.api.refresh_devices()?;
        let mut found = Vec::new();
        for info in self.api.device_list() {
            if info.vendor_id() != LOGITECH_VID || info.usage_page() < 0xFF00 {
                continue;
            }
            // Receivers expose one collection per report size; only the long one is useful.
            if info.usage_page() == 0xFF00 && info.usage() != 0x0002 {
                continue;
            }
            let Ok(dev) = self.api.open_path(info.path()) else { continue };
            let indices: &[u8] =
                if info.usage_page() == 0xFF00 { &[0xFF, 1, 2, 3, 4, 5, 6] } else { &[0xFF] };
            for &idx in indices {
                let feat = match feature_index(&dev, idx, FEATURE_CHANGE_HOST, PROBE_TIMEOUT) {
                    Ok(f) if f != 0 => f,
                    _ => continue,
                };
                let (mut name, kind) = device_name(&dev, idx).unwrap_or_default();
                if name.is_empty() {
                    name = info.product_string().unwrap_or("").trim().to_string();
                }
                if name.is_empty() {
                    name = format!("Logitech device {:04X}", info.product_id());
                }
                found.push(Target {
                    path: info.path().to_owned(),
                    dev_idx: idx,
                    feat_idx: feat,
                    name,
                    device_type: kind,
                    transport: transport(info, idx),
                });
            }
        }
        Ok(found)
    }

    fn choose(&self, found: Vec<Target>) -> Option<Target> {
        let mut candidates: Vec<Target> = match &self.filter {
            Some(f) => {
                let f = f.to_ascii_lowercase();
                found.into_iter().filter(|t| t.name.to_ascii_lowercase().contains(&f)).collect()
            }
            None => found,
        };
        candidates.sort_by_key(|t| {
            if t.is_mouse() {
                0
            } else if t.is_keyboard() {
                2
            } else {
                1
            }
        });
        candidates.into_iter().next()
    }

    fn resolve(&mut self) -> Result<(Target, HidDevice)> {
        if let Some(t) = self.cached.clone() {
            if let Ok(dev) = self.api.open_path(&t.path) {
                if host_info(&dev, &t, PROBE_TIMEOUT).is_ok() {
                    return Ok((t, dev));
                }
            }
            self.cached = None;
        }
        let found = self.discover()?;
        let missing = match &self.filter {
            Some(f) => format!("No Logitech device matching \"{f}\" with Easy-Switch support found on this PC."),
            None => "No Logitech device with Easy-Switch support found on this PC. Move the mouse to wake it and try again.".to_string(),
        };
        let t = self.choose(found).ok_or(Error::NotFound(missing))?;
        let dev = self.api.open_path(&t.path)?;
        self.cached = Some(t.clone());
        Ok((t, dev))
    }

    pub fn current(&mut self) -> Result<(Target, HostInfo)> {
        let (t, dev) = self.resolve()?;
        let info = host_info(&dev, &t, REPLY_TIMEOUT)?;
        Ok((t, info))
    }

    /// Switch to a channel (zero-based).
    pub fn set_host(&mut self, host: u8) -> Result<Outcome> {
        let (t, dev) = self.resolve()?;
        let info = host_info(&dev, &t, REPLY_TIMEOUT)?;
        switch(&dev, &t, info, host)
    }

    /// Switch to whichever of the two channels (zero-based) is not the current one.
    pub fn toggle(&mut self, a: u8, b: u8) -> Result<Outcome> {
        let (t, dev) = self.resolve()?;
        Self::toggle_with(&t, &dev, a, b)
    }

    /// Like `toggle`, on a handle the caller already holds.
    pub fn toggle_with(t: &Target, dev: &HidDevice, a: u8, b: u8) -> Result<Outcome> {
        let info = host_info(dev, t, REPLY_TIMEOUT)?;
        let target = if info.current == a { b } else { a };
        switch(dev, t, info, target)
    }

    /// Like `set_host`, on a handle the caller already holds.
    pub fn set_host_with(t: &Target, dev: &HidDevice, host: u8) -> Result<Outcome> {
        let info = host_info(dev, t, REPLY_TIMEOUT)?;
        switch(dev, t, info, host)
    }

    /// The preferred device and an open handle to it, validating the cached one first.
    pub fn open(&mut self) -> Result<(Target, HidDevice)> {
        self.resolve()
    }

    pub fn controls_text(&mut self) -> String {
        let (t, dev) = match self.resolve() {
            Ok(x) => x,
            Err(e) => return format!("{e}\n"),
        };
        let list = match controls(&dev, &t) {
            Ok(c) => c,
            Err(e) => return format!("{}: could not read the button list: {e}\n", t.name),
        };
        if list.is_empty() {
            return format!("{}: this device has no reprogrammable buttons (feature 0x1B04 missing).\n", t.name);
        }
        let mut out = format!("{} – buttons that can trigger a switch (use --button <id>):\n", t.name);
        for c in list {
            let name = control_name(c.cid);
            out += &format!(
                "  0x{:04X}  {:<24} {}\n",
                c.cid,
                if name.is_empty() { "(unnamed control)" } else { name },
                if c.divertable() { "ok" } else { "not divertable" }
            );
        }
        out
    }

    pub fn status_text(&mut self) -> String {
        let found = match self.discover() {
            Ok(f) => f,
            Err(e) => return format!("Error: {e}\n"),
        };
        if found.is_empty() {
            return "No Logitech device with Easy-Switch support found on this PC.\n\
                    (Move the mouse to wake it, then try again. Run `mxswitch list` to see what Windows exposes.)\n"
                .to_string();
        }
        let chosen = self.choose(found.clone());
        let mut out = String::new();
        for t in &found {
            let selected = chosen.as_ref().is_some_and(|c| c.same_as(t));
            out += &format!(
                "{}{}\n  via {} – HID++ device index 0x{:02X}, ChangeHost feature index 0x{:02X}\n",
                t.name,
                if selected { "  (selected)" } else { "" },
                t.transport,
                t.dev_idx,
                t.feat_idx
            );
            let dev = match self.api.open_path(&t.path) {
                Ok(d) => d,
                Err(e) => {
                    out += &format!("  cannot open: {e}\n");
                    continue;
                }
            };
            match host_info(&dev, t, REPLY_TIMEOUT) {
                Ok(info) => {
                    out += &format!("  channels: {}, currently on {}\n", info.count, info.current + 1);
                    let slots = host_slots(&dev, t).unwrap_or_default();
                    for h in 0..info.count {
                        let label = match slots.iter().find(|s| s.index == h) {
                            Some(s) if s.paired && !s.name.is_empty() => s.name.clone(),
                            Some(s) if s.paired => "(paired)".to_string(),
                            Some(_) => "(empty)".to_string(),
                            None => String::new(),
                        };
                        let here = h == info.current;
                        out += &format!(
                            "    {} {}: {}{}\n",
                            if here { ">" } else { " " },
                            h + 1,
                            label,
                            if here { "  <- this PC" } else { "" }
                        );
                    }
                }
                Err(e) => out += &format!("  could not read channel info: {e}\n"),
            }
        }
        out
    }

    pub fn list_collections(&mut self) -> String {
        let mut out = String::from("usagePage:usage  pid   bus        product\n");
        if let Err(e) = self.api.refresh_devices() {
            return format!("Error: {e}\n");
        }
        let mut n = 0;
        for info in self.api.device_list() {
            if info.vendor_id() != LOGITECH_VID || info.usage_page() < 0xFF00 {
                continue;
            }
            n += 1;
            out += &format!(
                "  {:04x}:{:04x}      {:04x}  {:<9}  {}\n",
                info.usage_page(),
                info.usage(),
                info.product_id(),
                bus_name(info.bus_type()),
                info.product_string().unwrap_or("")
            );
        }
        if n == 0 {
            out += "  (no Logitech vendor-defined HID collections found)\n";
        }
        out
    }
}
