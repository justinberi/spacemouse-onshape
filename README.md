# spacemouse-onshape

Use a 3Dconnexion SpaceMouse with [Onshape](https://www.onshape.com) on Linux.

On Windows and macOS, 3Dconnexion's driver (3DxWare) runs a small local HTTPS/WebSocket server
that the JavaScript inside Onshape connects to. There is no 3DxWare for Linux, so Onshape never
finds a 3D mouse there. `spacemouse-onshape` is that server for Linux: one small Rust binary
that reads the device directly through `hidraw`. You don't need `spacenavd`, Python or a
proprietary driver.

- Smooth, frame-rate-independent motion: the cap's deflection sets a speed, as with 3DxWare.
- All six axes at once. Orbits about the pivot Onshape chooses, in orthographic or perspective views.
- Buttons for standard views and fit.
- Coexists with SolidCraft, FreeCAD, Blender and `spacenavd`. `hidraw` gives every reader its own
  copy of the reports, and only the focused Onshape tab moves.
- A local certificate authority that is name-constrained to loopback, and whose private key is
  thrown away after one use.

Should work with any 3Dconnexion 6-DoF device: SpaceMouse Compact, Wireless, Pro and
Enterprise, SpaceNavigator, SpacePilot, SpaceExplorer, and the Universal Receiver. Wired and
wireless report layouts are both handled.

## Install

Installing takes two steps: the program, and a browser extension (Chrome doesn't let programs
install extensions for you).

### 1. The program

**Debian, Ubuntu, Pop!_OS, Mint**: download the `.deb` from
[Releases](https://github.com/justinberi/spacemouse-onshape/releases) and install it:

```sh
sudo apt install ./spacemouse-onshape_*.deb
systemctl --user start spacemouse-onshape    # or log out and back in
```

The package installs:
- a udev rule, so you can read the device;
- a systemd user service, which starts the bridge at every login;
- a step at startup that makes Chrome and Firefox trust the bridge's certificate.

**Anything else, with Rust**:

```sh
cargo install --git https://github.com/justinberi/spacemouse-onshape
spacemouse-onshape setup
```

`setup` does the same as the package. It asks for `sudo` once, for the udev rule. For
certificate trust it needs `certutil`: on Debian/Ubuntu that's `libnss3-tools`, on Fedora
`nss-tools`, on Arch `nss`. `spacemouse-onshape uninstall` undoes it.

If the SpaceMouse was plugged in during installation and `spacemouse-onshape monitor` can't read it,
replug it once. Restart the browser so it picks up the certificate.

### 2. The browser extension

Onshape only looks for a 3D mouse driver when it thinks it's running on Windows or macOS. The
extension sets `navigator.platform` to `Win32` on onshape.com, and does nothing else.

- **Chrome, Chromium, Brave, Edge**: open `chrome://extensions`, turn on *Developer mode*, choose
  *Load unpacked*, and select `~/.local/share/spacemouse-onshape/extension`. The bridge writes the
  extension there; it's also in [`extension/`](extension/).
- **Firefox, or any browser with Tampermonkey/Violentmonkey**: open
  <https://127.51.68.120:8181/onshape-spacemouse.user.js> to install the userscript.

Then open an Onshape document, click into the graphics area and move the cap. If Chrome asks
whether Onshape may access devices on your local network, allow it. <https://127.51.68.120:8181>
shows the device, the live axes, and which Onshape tabs are connected.

### About the certificate

Onshape is served over HTTPS, so the browser will only connect to the bridge over HTTPS. On first
run the bridge makes its own certificate authority (CA) and signs its server certificate with it.
The CA:
- can only vouch for `127.0.0.0/8` and `localhost` (an X.509 name constraint);
- has its private key thrown away after that one signature.

So trusting it does not let anyone, including this program, issue certificates for real sites.
Without `certutil`, open <https://127.51.68.120:8181> and accept the warning instead.

## Options

```text
spacemouse-onshape [serve] [OPTIONS]
  --mode object|camera     the model follows the cap (default), or the camera does
  --speed 1.0              multiplies all speeds
  --pan-speed 1.0          view heights per second at full deflection
  --rotate-speed 2.5       radians per second at full deflection
  --zoom-speed 1.5         e-folds per second at full deflection
  --deadzone 0.04          fraction of travel ignored around rest
  --curve 1.6              response exponent (1 = linear; higher = finer near rest)
  --invert AXES            e.g. --invert zoom,roll  (pan-x pan-y zoom pitch yaw roll)
  --no-rotate / --no-translate
  --buttons front,fit      actions for buttons 1, 2, ...: fit front back left right top bottom iso none
  --frame-rate 60          camera updates per second while moving
  --device /dev/hidrawN    use this device instead of searching
  --allow-origin ORIGIN    also accept another site using 3Dconnexion's web library
  --trust-browsers         at startup, add the CA to browsers that don't trust it yet

spacemouse-onshape setup        one-time setup (udev rule, certificate, login service, extension)
spacemouse-onshape uninstall    undo setup
spacemouse-onshape monitor      print the device's axes and buttons
spacemouse-onshape cert         print the CA's path   (--install: add it to browsers)
spacemouse-onshape userscript   print the userscript
```

To change options for the service, run `systemctl --user edit spacemouse-onshape` and override
`ExecStart`. Logs: `journalctl --user -u spacemouse-onshape`. To log the protocol, set
`RUST_LOG=spacemouse_onshape=debug`.

## How it works

3Dconnexion's browser library (which Onshape embeds) fetches
`https://127.51.68.120:8181/3dconnexion/nlproxy` to learn the WebSocket port. It then opens a
WebSocket with subprotocol `wamp` and speaks WAMP v1:

1. The server sends `WELCOME`. The page sends `PREFIX` messages, then `CALL`s
   `3dx_rpc:create` for a `3dmouse` and a `3dcontroller`. It then `SUBSCRIBE`s to the controller's
   topic and sends `3dx_rpc:update {"focus": true|false}` as its window gains and loses focus.
2. To move the camera, the server calls into the page: it publishes `EVENT`s on the controller
   topic, each wrapping a `CALL` such as `self:read view.affine` or
   `self:update view.affine [...]`. The page answers with `CALLRESULT` or `CALLERROR`.

Each motion reads `view.perspective`, `view.affine` (camera-to-world, column-major), and
`view.extents` (orthographic) or `view.frustum` (perspective). It also reads `pivot.position`
and sets `motion`. Each frame then writes `view.affine`, and `view.extents` when an orthographic
zoom changes it, inside a `transaction`. Onshape redraws when the transaction returns to 0.

The device is read from `/dev/hidrawN`. Report 1 carries translation (wireless devices put all six
axes in report 1), report 2 carries rotation, and report 3 the buttons. Each value is a signed
16-bit number, about ±350 at full deflection.

## SolidCraft

[SolidCraft](https://github.com/tangency-labs/solidcraft) supports 3D mice on Linux itself,
through `spacenavd` or directly through `hidraw`. The udev rule above is all it needs; this
program isn't involved. Both can run at once.

## Troubleshooting

- **`monitor` says no permission**: the udev rule isn't installed, or the device needs replugging.
- **Port 8181 is in use**: the bridge is already running (as the service, probably). Stop it
  with `systemctl --user stop spacemouse-onshape`.
- **Nothing happens in Onshape**: open <https://127.51.68.120:8181>. If the page loads, the
  certificate is fine, and it lists connected pages. No pages listed means the userscript or
  extension is not active (reload the Onshape tab after installing it). A listed page that isn't
  "focused" needs a click inside Onshape's graphics area.
- **An axis goes the wrong way**: use `--invert`. The status page shows each axis live.

## Credits

The protocol notes owe a lot to [spacenav-ws](https://github.com/RmStorm/spacenav-ws), whose
authors first worked out Onshape's side of it. This is an independent implementation.

3Dconnexion, SpaceMouse and 3DxWare are trademarks of 3Dconnexion. Onshape is a trademark of PTC
Inc. This project is not affiliated with or endorsed by either.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
