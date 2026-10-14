# mxswitch

Move a Logitech Easy-Switch mouse — MX Master 4 / 3S, MX Anywhere, Lift, and friends —
to your other PC with **one press of the mouse's own gesture (thumb) button**, instead of
flipping it over and pressing the channel button underneath.

A single 300 KB `mxswitch.exe` for Windows. No Logi Options+ needed, no drivers, no admin
rights, no AutoHotkey. It sits in the tray, takes over the gesture button, and sends the
mouse to the other channel when you press it. Run it on both PCs and the same button bounces
the mouse back and forth.

```
on PC 1, press gesture button ──▶ mouse connects to PC 2
on PC 2, press gesture button ──▶ mouse connects to PC 1
```

## Install

Download `mxswitch.exe` from the [latest release](https://github.com/shivajreddy/mxswitch/releases/latest),
put it somewhere permanent (e.g. `C:\Tools\mxswitch.exe`), then run once:

```
mxswitch.exe --install
```

That registers it to start at login and starts the tray icon right away. **Do this on every
PC the mouse is paired with** — a switch is always sent from the PC that currently has the
mouse, so the other PC needs its own copy to send it back.

Or, with a Rust toolchain: `cargo install mxswitch`.

## Use

Press the gesture button. Done.

The tray icon (a mouse) also offers: which channel the mouse is on right now, *Switch to the
other PC*, direct *Channel 1 / 2 / 3* entries, *Status…*, *Start with Windows*, and *Exit*.
Double-clicking the icon switches too.

**If Logi Options+ is installed**, set the gesture button to **No action** there (on each
PC), otherwise Options+ keeps reacting to it as well — on the MX Master 4 that means the
Actions Ring pops up right before the mouse leaves. Options+ and mxswitch coexist fine
otherwise.

### Other triggers

Don't want to give up the gesture button? Any of these work:

```
mxswitch.exe --install --button back        # or forward, middle, smartshift, 0x00C3 ...
mxswitch.exe --install --button none --key ctrl+alt+m   # keyboard hotkey instead
mxswitch.exe --install --key F21            # gesture button AND a hotkey
mxswitch.exe controls                       # list the buttons your mouse can hand over
```

A hotkey like `F21` is handy when some *other* software should trigger the switch — Logi
Options+ keystroke assignments, a macro pad, a keyboard remapper — because no real keyboard
has that key.

### Command line

```
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
```

`status` output looks like this:

```
MX Master 4  (selected)
  via Bluetooth – HID++ device index 0xFF, ChangeHost feature index 0x0E
  channels: 3, currently on 1
    > 1: (paired)  <- this PC
      2: (paired)
      3: (paired)
```

## Good to know

- **Keep the other PC awake.** If you switch to a channel whose PC is asleep, off, or has
  Bluetooth disabled, the mouse parks on a dead channel and the button underneath is the
  only way back. A Bolt/Unifying receiver stays live whenever the PC is powered, which
  makes it the more forgiving option.
- Works whether the mouse is connected over **Bluetooth** or through a **Logi Bolt /
  Unifying receiver**; it detects both at runtime.
- If a Logitech keyboard with Easy-Switch is also present, the **mouse is preferred**. Use
  `--device Keys` (or any substring of the name) to target something else.
- The button hand-over is volatile: the mouse forgets it every time it reconnects. mxswitch
  notices the mouse coming back (within a couple of seconds) and re-arms the button; it also
  hands the button back when you exit the tray app.
- A switch is confirmed by absence: after sending the command the tool checks that the mouse
  has actually stopped answering, retrying a few times if needed, so a failure is reported
  (tray notification) rather than guessed.

## How it works

Logitech devices speak **HID++**, a request/response protocol tunnelled through a
vendor-defined HID collection next to the ordinary mouse reports. Two features do all the
work:

- `0x1814` *ChangeHost* — `setCurrentHost(index)` does exactly what the channel button does.
- `0x1B04` *ReprogControlsV4* — "diverting" a button makes the mouse report presses to
  software instead of acting on them; this is the same mechanism Logi Options+ uses for its
  button customisation.

`mxswitch` resolves where those features sit in the device's feature table at runtime (it
differs per device and firmware), diverts the trigger button, and on release reads the
current channel and sends the switch. Everything else — the tray icon, the optional global
hotkey, the autostart entry — is plain Win32.

## Credits

- [marcocosta97/mxswitch](https://github.com/marcocosta97/mxswitch) — the PowerShell /
  Python / C implementations that proved the approach; the verify-by-absence trick is
  borrowed from there.
- [Solaar](https://github.com/pwr-Solaar/Solaar) — the most complete public documentation of
  HID++ features.
- [hidapi](https://github.com/libusb/hidapi) via the [`hidapi`](https://crates.io/crates/hidapi) crate.

## License

MIT
