//! The listener thread behind the tray app. It owns the connection to the mouse:
//! finds it, diverts the trigger button so presses are reported to us, switches
//! hosts on release, notices when the mouse leaves or comes back (re-diverting
//! each time, because the device forgets diverts on reconnect), and serves the
//! tray's requests. Everything that touches HID happens on this one thread.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hidapi::HidDevice;

use crate::hidpp::{self, control_name, HostInfo, Outcome, Switcher, Target};

pub enum Command {
    Toggle,
    SetHost(u8),
    Current(Sender<hidpp::Result<(Target, HostInfo)>>),
    Status(Sender<String>),
    Quit,
}

pub enum Event {
    Connected { name: String, channel: u8 },
    Disconnected,
    Notice { title: String, text: String },
}

pub struct Config {
    pub hosts: (u8, u8),
    pub device: Option<String>,
    pub button: Option<u16>,
}

/// How the thread hands events to the UI thread.
pub trait Sink: Send + 'static {
    fn post(&self, event: Event);
}

pub struct Handle {
    pub tx: Sender<Command>,
    pub thread: JoinHandle<()>,
}

const PROBE_EVERY: Duration = Duration::from_millis(1500);
const CHECK_DIVERT_EVERY: Duration = Duration::from_secs(3);

struct Live {
    target: Target,
    dev: HidDevice,
    reprog: u8,
    held: bool,
}

pub fn spawn(cfg: Config, sink: impl Sink) -> Handle {
    let (tx, rx) = channel();
    let thread = thread::spawn(move || run(cfg, rx, sink));
    Handle { tx, thread }
}

fn run(cfg: Config, rx: Receiver<Command>, sink: impl Sink) {
    let mut switcher = match Switcher::new(cfg.device.clone()) {
        Ok(s) => s,
        Err(e) => {
            sink.post(Event::Notice { title: "MxSwitch".into(), text: format!("Cannot initialise HID access: {e}") });
            return;
        }
    };
    let mut live: Option<Live> = None;
    let mut next_probe = Instant::now();
    let mut next_check = Instant::now();
    let mut buf = [0u8; 64];

    loop {
        // Commands from the tray. While disconnected this is also where we wait.
        let cmd = if live.is_some() {
            rx.try_recv().ok()
        } else {
            let wait = next_probe.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait.max(Duration::from_millis(10))) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        if let Some(cmd) = cmd {
            match cmd {
                Command::Quit => {
                    release(&mut live, cfg.button);
                    return;
                }
                Command::Toggle => {
                    let r = match &live {
                        Some(l) => Switcher::toggle_with(&l.target, &l.dev, cfg.hosts.0, cfg.hosts.1),
                        None => switcher.toggle(cfg.hosts.0, cfg.hosts.1),
                    };
                    after_switch(r, &mut live, &sink);
                }
                Command::SetHost(h) => {
                    let r = match &live {
                        Some(l) => Switcher::set_host_with(&l.target, &l.dev, h),
                        None => switcher.set_host(h),
                    };
                    after_switch(r, &mut live, &sink);
                }
                Command::Current(reply) => {
                    let r = match &live {
                        Some(l) => hidpp::host_info(&l.dev, &l.target, Duration::from_millis(800))
                            .map(|info| (l.target.clone(), info)),
                        None => switcher.current(),
                    };
                    let _ = reply.send(r);
                }
                Command::Status(reply) => {
                    let _ = reply.send(switcher.status_text());
                }
            }
            continue;
        }

        if live.is_none() {
            if Instant::now() >= next_probe {
                next_probe = Instant::now() + PROBE_EVERY;
                if let Some(l) = connect(&mut switcher, cfg.button, &sink) {
                    live = Some(l);
                    next_check = Instant::now() + CHECK_DIVERT_EVERY;
                }
            }
            continue;
        }

        let l = live.as_mut().expect("live");
        let trigger = if l.reprog != 0 { cfg.button } else { None };
        match l.dev.read_timeout(&mut buf, 100) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(button) = trigger {
                    if let Some(cids) = hidpp::diverted_buttons_event(&buf[..n], &l.target, l.reprog) {
                        let down = cids.contains(&button);
                        let released = l.held && !down;
                        l.held = down;
                        if released {
                            let r = Switcher::toggle_with(&l.target, &l.dev, cfg.hosts.0, cfg.hosts.1);
                            after_switch(r, &mut live, &sink);
                            continue;
                        }
                    }
                }
            }
            Err(_) => {
                live = None;
                sink.post(Event::Disconnected);
                continue;
            }
        }

        // The device drops diverts when it reconnects, and other software may reset them.
        if let Some(button) = trigger {
            if Instant::now() >= next_check {
                next_check = Instant::now() + CHECK_DIVERT_EVERY;
                match hidpp::is_diverted(&l.dev, &l.target, l.reprog, button) {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = hidpp::set_divert(&l.dev, &l.target, l.reprog, button, true);
                    }
                    Err(_) => {
                        live = None;
                        sink.post(Event::Disconnected);
                    }
                }
            }
        }
    }
}

fn connect(switcher: &mut Switcher, button: Option<u16>, sink: &impl Sink) -> Option<Live> {
    let (target, dev) = switcher.open().ok()?;
    let info = hidpp::host_info(&dev, &target, Duration::from_millis(800)).ok()?;
    let mut reprog = 0;
    if let Some(button) = button {
        reprog = hidpp::reprog_feature(&dev, &target).unwrap_or(0);
        let divertable = reprog != 0
            && hidpp::controls(&dev, &target)
                .map(|list| list.iter().any(|c| c.cid == button && c.divertable()))
                .unwrap_or(false);
        if !divertable {
            sink.post(Event::Notice {
                title: "Button not available".into(),
                text: format!(
                    "{} cannot hand over button 0x{:04X} {}. Run `mxswitch controls` to see the options, or use --key for a hotkey.",
                    target.name,
                    button,
                    control_name(button)
                ),
            });
            reprog = 0;
        } else if let Err(e) = hidpp::set_divert(&dev, &target, reprog, button, true) {
            sink.post(Event::Notice { title: "Button not available".into(), text: format!("{}: {e}", target.name) });
            reprog = 0;
        }
    }
    sink.post(Event::Connected { name: target.name.clone(), channel: info.current });
    Some(Live { target, dev, reprog, held: false })
}

fn release(live: &mut Option<Live>, button: Option<u16>) {
    if let (Some(l), Some(button)) = (live.take(), button) {
        if l.reprog != 0 {
            let _ = hidpp::set_divert(&l.dev, &l.target, l.reprog, button, false);
        }
    }
}

fn after_switch(result: hidpp::Result<Outcome>, live: &mut Option<Live>, sink: &impl Sink) {
    match result {
        Ok(Outcome::Switched { .. }) => {
            // The mouse is gone; the next probe picks it up again when it returns.
            *live = None;
            sink.post(Event::Disconnected);
        }
        Ok(Outcome::AlreadyThere { name, host }) => {
            sink.post(Event::Notice { title: "Nothing to do".into(), text: format!("{name} is already on channel {}.", host + 1) });
        }
        Err(e) => {
            sink.post(Event::Notice { title: "Mouse switch failed".into(), text: e.to_string() });
        }
    }
}
