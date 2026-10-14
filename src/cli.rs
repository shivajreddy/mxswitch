//! Argument parsing and dispatch: tray mode with no arguments, CLI otherwise (see USAGE).

use std::process::exit;

use crate::hidpp::{self, Outcome, Switcher, CID_GESTURE};
use crate::win;

const USAGE: &str = "\
MxSwitch - move a Logitech Easy-Switch mouse to the other PC with one press

  mxswitch.exe                 run in the tray; the mouse's gesture button moves it to the other PC
  mxswitch.exe toggle          move the mouse to the other PC now
  mxswitch.exe 1 | 2 | 3       move the mouse to a specific channel
  mxswitch.exe status          show the mouse, its channels and which one is active
  mxswitch.exe controls        list the mouse buttons that can be used as the trigger
  mxswitch.exe list            show the raw Logitech HID collections (for debugging)
  mxswitch.exe --install       start the tray app at login (and start it now)
  mxswitch.exe --uninstall     stop starting the tray app at login

Options (tray mode and --install):
  --button <name>    mouse button that triggers the switch: gesture (default), back,
                     forward, middle, smartshift, a control id like 0x00C3, or none
  --key <hotkey>     also trigger on a keyboard hotkey, e.g. --key F21, --key ctrl+alt+m
  --hosts <a,b>      the two channels to toggle between, default 1,2
  --device <name>    only act on a device whose name contains <name>

Install the same exe on every PC the mouse is paired with: a switch is always
sent from the PC that currently has the mouse.
";

enum Cmd {
    Tray,
    Status,
    Controls,
    List,
    Toggle,
    Host(u8),
    Install,
    Uninstall,
    Help,
}

struct Opts {
    cmd: Cmd,
    key: Option<String>,
    button: Option<u16>,
    hosts: (u8, u8),
    device: Option<String>,
}

fn parse_hosts(v: &str) -> Result<(u8, u8), String> {
    let bad = || format!("--hosts expects two different channels 1-3, e.g. --hosts 1,2 (got '{v}')");
    let parts: Vec<u8> = v
        .split([',', '/', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u8>().map_err(|_| bad()))
        .collect::<Result<_, _>>()?;
    match parts[..] {
        [a, b] if (1..=3).contains(&a) && (1..=3).contains(&b) && a != b => Ok((a - 1, b - 1)),
        _ => Err(bad()),
    }
}

fn parse_args(args: Vec<String>) -> Result<Opts, String> {
    let mut opts = Opts { cmd: Cmd::Tray, key: None, button: Some(CID_GESTURE), hosts: (0, 1), device: None };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.to_ascii_lowercase().as_str() {
            "status" | "info" => opts.cmd = Cmd::Status,
            "controls" | "buttons" => opts.cmd = Cmd::Controls,
            "list" => opts.cmd = Cmd::List,
            "toggle" => opts.cmd = Cmd::Toggle,
            "1" | "2" | "3" => opts.cmd = Cmd::Host(arg.parse::<u8>().unwrap() - 1),
            "--install" | "install" => opts.cmd = Cmd::Install,
            "--uninstall" | "uninstall" => opts.cmd = Cmd::Uninstall,
            "-h" | "--help" | "help" | "/?" => opts.cmd = Cmd::Help,
            "--key" | "-k" => {
                let v = it.next().ok_or("--key needs a value, e.g. --key F21")?;
                opts.key = if v.eq_ignore_ascii_case("none") { None } else { Some(v) };
            }
            "--button" | "-b" => {
                let v = it.next().ok_or("--button needs a value, e.g. --button gesture")?;
                opts.button = if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(hidpp::parse_button(&v).ok_or_else(|| {
                        format!("unknown button '{v}' (try gesture, back, forward, middle, smartshift, 0x00C3 or none)")
                    })?)
                };
            }
            "--hosts" => opts.hosts = parse_hosts(&it.next().ok_or("--hosts needs a value, e.g. --hosts 1,2")?)?,
            "--device" | "-d" => {
                opts.device = Some(it.next().ok_or("--device needs a name, e.g. --device \"MX Master\"")?)
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(opts)
}

/// The non-default options, so --install and the tray's "Start with Windows" preserve them.
fn extra_args(opts: &Opts) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(k) = &opts.key {
        v.push("--key".into());
        v.push(k.clone());
    }
    if opts.button != Some(CID_GESTURE) {
        v.push("--button".into());
        v.push(opts.button.map(|b| format!("0x{b:04X}")).unwrap_or_else(|| "none".into()));
    }
    if opts.hosts != (0, 1) {
        v.push("--hosts".into());
        v.push(format!("{},{}", opts.hosts.0 + 1, opts.hosts.1 + 1));
    }
    if let Some(d) = &opts.device {
        v.push("--device".into());
        v.push(d.clone());
    }
    v
}

fn outcome_text(result: hidpp::Result<Outcome>) -> (String, i32) {
    match result {
        Ok(Outcome::Switched { name, from, to }) => {
            (format!("{name}: switched from channel {} to channel {}.\n", from + 1, to + 1), 0)
        }
        Ok(Outcome::AlreadyThere { name, host }) => (format!("{name} is already on channel {}.\n", host + 1), 0),
        Err(e) => (format!("Switch failed: {e}\n"), 1),
    }
}

fn fail(msg: &str, code: i32) -> ! {
    win::emit(&format!("{msg}\n"), "MxSwitch", true);
    exit(code);
}

pub fn run() -> ! {
    let opts = match parse_args(std::env::args().skip(1).collect()) {
        Ok(o) => o,
        Err(e) => fail(&format!("{e}\n\n{USAGE}"), 2),
    };
    let hotkey = match opts.key.as_deref().map(win::parse_hotkey).transpose() {
        Ok(h) => h,
        Err(e) => fail(&e, 2),
    };
    let extra = extra_args(&opts);
    let trigger = match (&opts.button, &hotkey) {
        (Some(b), _) => {
            let name = hidpp::control_name(*b);
            if name.is_empty() { format!("mouse button 0x{b:04X}") } else { format!("the {name} on the mouse") }
        }
        (None, Some(k)) => k.label.clone(),
        (None, None) => "the tray menu".to_string(),
    };

    match opts.cmd {
        Cmd::Tray => exit(win::run_tray(win::TrayConfig {
            hotkey,
            button: opts.button,
            hosts: opts.hosts,
            device: opts.device,
            extra_args: extra,
        })),
        Cmd::Help => {
            win::emit(USAGE, "MxSwitch", false);
            exit(0);
        }
        Cmd::Install => {
            let cmd = match win::set_autostart(true, &extra) {
                Ok(cmd) => cmd,
                Err(e) => fail(&e, 1),
            };
            let mut text = format!("MxSwitch will start at login:\n  {cmd}\n");
            if win::is_tray_running() {
                text += "The tray app is already running.\n";
            } else {
                // Detach the tray app's stdio: inheriting a pipe would keep the caller waiting.
                let started = std::env::current_exe().and_then(|exe| {
                    std::process::Command::new(exe)
                        .args(&extra)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                });
                match started {
                    Ok(_) => text += &format!("Started the tray app now – press {trigger} to switch.\n"),
                    Err(e) => text += &format!("Could not start the tray app now: {e}\n"),
                }
            }
            win::emit(&text, "MxSwitch", false);
            exit(0);
        }
        Cmd::Uninstall => {
            if let Err(e) = win::set_autostart(false, &extra) {
                fail(&e, 1);
            }
            win::emit("MxSwitch will no longer start at login (exit the tray icon to stop it now).\n", "MxSwitch", false);
            exit(0);
        }
        Cmd::Status | Cmd::Controls | Cmd::List | Cmd::Toggle | Cmd::Host(_) => {
            let mut switcher = match Switcher::new(opts.device.clone()) {
                Ok(s) => s,
                Err(e) => fail(&format!("Cannot initialise HID access: {e}"), 1),
            };
            let (text, code) = match opts.cmd {
                Cmd::Status => (switcher.status_text(), 0),
                Cmd::Controls => (switcher.controls_text(), 0),
                Cmd::List => (switcher.list_collections(), 0),
                Cmd::Toggle => outcome_text(switcher.toggle(opts.hosts.0, opts.hosts.1)),
                Cmd::Host(h) => outcome_text(switcher.set_host(h)),
                _ => unreachable!(),
            };
            win::emit(&text, "MxSwitch", code != 0);
            exit(code);
        }
    }
}
