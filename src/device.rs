//! Reads a 3Dconnexion 6-DoF device straight from Linux `hidraw`.
//!
//! No driver or daemon is needed: the device is found under `/sys/class/hidraw`, and its reports
//! are read from `/dev/hidrawN`. `hidraw` hands every report to every reader, so this coexists
//! with `spacenavd`, SolidCraft, FreeCAD or anything else reading the same device.
//!
//! The user needs read access to the device node; see `packaging/70-spacemouse.rules`.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;

/// 3Dconnexion's own USB vendor id.
const VENDOR_3DCONNEXION: u32 = 0x256f;
/// Logitech's vendor id, used by 3Dconnexion devices made before 2011.
const VENDOR_LOGITECH: u32 = 0x046d;
/// 3Dconnexion products sold under Logitech's vendor id.
const LOGITECH_3D_PRODUCTS: &[u32] = &[
    0xc603, // SpaceMouse Plus XT
    0xc605, // CADman
    0xc606, // SpaceMouse Classic
    0xc621, // SpaceBall 5000
    0xc623, // SpaceTraveler
    0xc625, // SpacePilot
    0xc626, // SpaceNavigator
    0xc627, // SpaceExplorer
    0xc628, // SpaceNavigator for Notebooks
    0xc629, // SpacePilot Pro
    0xc62b, // SpaceMouse Pro
];

/// The HID report descriptor items "Usage Page (Generic Desktop), Usage (Multi-axis Controller)".
/// Only interfaces that declare it are 6-DoF devices; this skips e.g. a CadMouse that shares a
/// 3Dconnexion Universal Receiver with a SpaceMouse.
const MULTI_AXIS_USAGE: [u8; 4] = [0x05, 0x01, 0x09, 0x08];

const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// The latest state of the device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceState {
    /// Whether a device is open and being read.
    pub connected: bool,
    /// The device's HID name, e.g. "3Dconnexion SpaceMouse Compact".
    pub name: Option<String>,
    /// Raw HID axes `[x, y, z, rx, ry, rz]`, about ±350 at full deflection.
    ///
    /// In the device's own frame: x to the right, y toward the user, z down (into the desk), with
    /// rotations right-handed about those axes.
    pub axes: [i16; 6],
    /// One bit per button, bit 0 being the first (the left button on a SpaceMouse Compact).
    pub buttons: u32,
    /// Why no device is connected, when there is a reason worth showing.
    pub problem: Option<String>,
}

/// A candidate device found in sysfs.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub node: PathBuf,
    pub name: String,
}

/// Applies one input report to `state`. Returns whether anything changed.
///
/// 3Dconnexion devices use a fixed layout:
/// - report 1: translation, three little-endian `i16` (wired devices), or all six axes (wireless)
/// - report 2: rotation, three `i16`
/// - report 3: buttons, a little-endian bitmask
pub fn apply_report(state: &mut DeviceState, report: &[u8]) -> bool {
    let before = (state.axes, state.buttons);
    match report.first() {
        Some(1) if report.len() >= 13 => read_axes(&mut state.axes, &report[1..13]),
        Some(1) if report.len() >= 7 => read_axes(&mut state.axes[..3], &report[1..7]),
        Some(2) if report.len() >= 7 => read_axes(&mut state.axes[3..], &report[1..7]),
        Some(3) if report.len() >= 2 => {
            let mut bits = [0u8; 4];
            let n = (report.len() - 1).min(4);
            bits[..n].copy_from_slice(&report[1..1 + n]);
            state.buttons = u32::from_le_bytes(bits);
        }
        _ => {}
    }
    before != (state.axes, state.buttons)
}

/// Fills `axes` from consecutive little-endian `i16` values in `bytes`.
fn read_axes(axes: &mut [i16], bytes: &[u8]) {
    let (pairs, _) = bytes.as_chunks::<2>();
    for (axis, pair) in axes.iter_mut().zip(pairs) {
        *axis = i16::from_le_bytes(*pair);
    }
}

/// Parses `HID_ID=0003:0000256F:0000C635` into (vendor, product).
fn parse_hid_id(uevent: &str) -> Option<(u32, u32)> {
    let id = uevent.lines().find_map(|l| l.strip_prefix("HID_ID="))?;
    let mut parts = id.split(':');
    let _bus = parts.next()?;
    let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
    let product = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((vendor, product))
}

fn is_3d_mouse(vendor: u32, product: u32) -> bool {
    vendor == VENDOR_3DCONNEXION
        || (vendor == VENDOR_LOGITECH && LOGITECH_3D_PRODUCTS.contains(&product))
}

/// Finds 3D mice under `sysfs_root` (normally `/sys/class/hidraw`).
pub fn scan(sysfs_root: &Path) -> Vec<Found> {
    let Ok(entries) = std::fs::read_dir(sysfs_root) else {
        return Vec::new();
    };
    let mut found: Vec<Found> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let uevent = std::fs::read_to_string(dir.join("device/uevent")).ok()?;
            let (vendor, product) = parse_hid_id(&uevent)?;
            if !is_3d_mouse(vendor, product) {
                return None;
            }
            let descriptor = std::fs::read(dir.join("device/report_descriptor")).ok()?;
            if !descriptor.windows(4).any(|w| w == MULTI_AXIS_USAGE) {
                return None;
            }
            let name = uevent
                .lines()
                .find_map(|l| l.strip_prefix("HID_NAME="))
                .unwrap_or("3D mouse")
                .trim()
                .to_string();
            Some(Found {
                node: Path::new("/dev").join(entry.file_name()),
                name,
            })
        })
        .collect();
    found.sort_by(|a, b| a.node.cmp(&b.node));
    found
}

/// Starts a thread that keeps `tx` up to date with the device, reconnecting when it is
/// unplugged and plugged back in. `node` forces a particular `/dev/hidrawN`.
pub fn spawn_reader(tx: watch::Sender<DeviceState>, node: Option<PathBuf>) {
    std::thread::Builder::new()
        .name("hidraw".into())
        .spawn(move || reader_loop(&tx, node))
        .expect("spawn hidraw reader thread");
}

fn reader_loop(tx: &watch::Sender<DeviceState>, forced: Option<PathBuf>) {
    let mut last_problem: Option<String> = None;
    loop {
        let candidates = match &forced {
            Some(node) => vec![Found {
                node: node.clone(),
                name: node.display().to_string(),
            }],
            None => scan(Path::new("/sys/class/hidraw")),
        };
        let mut problem = if candidates.is_empty() {
            Some("no 3Dconnexion device found".to_string())
        } else {
            None
        };
        for found in candidates {
            match File::open(&found.node) {
                Ok(file) => {
                    tracing::info!("reading {} ({})", found.name, found.node.display());
                    last_problem = None;
                    read_device(tx, file, &found);
                    tracing::info!("{} disconnected", found.name);
                    problem = None;
                    break;
                }
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    problem = Some(format!(
                        "no permission to read {} ({}); install the udev rule from \
                         packaging/70-spacemouse.rules and replug the device",
                        found.node.display(),
                        found.name
                    ));
                }
                Err(e) => problem = Some(format!("cannot open {}: {e}", found.node.display())),
            }
        }
        if problem != last_problem {
            if let Some(p) = &problem {
                tracing::warn!("{p}");
            }
            last_problem = problem.clone();
        }
        tx.send_modify(|s| {
            *s = DeviceState {
                problem: problem.clone(),
                ..DeviceState::default()
            };
        });
        std::thread::sleep(RESCAN_INTERVAL);
    }
}

fn read_device(tx: &watch::Sender<DeviceState>, mut file: File, found: &Found) {
    let mut state = DeviceState {
        connected: true,
        name: Some(found.name.clone()),
        ..DeviceState::default()
    };
    tx.send_replace(state.clone());
    let mut buf = [0u8; 64];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if apply_report(&mut state, &buf[..n]) {
                    tx.send_replace(state.clone());
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::debug!("read {}: {e}", found.node.display());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wired_reports_fill_translation_rotation_and_buttons() {
        let mut s = DeviceState::default();
        // x = 10, y = -20, z = 350
        assert!(apply_report(&mut s, &[1, 10, 0, 0xec, 0xff, 0x5e, 0x01]));
        assert_eq!(s.axes, [10, -20, 350, 0, 0, 0]);
        // rx = -350, ry = 1, rz = 0
        assert!(apply_report(&mut s, &[2, 0xa2, 0xfe, 1, 0, 0, 0]));
        assert_eq!(s.axes, [10, -20, 350, -350, 1, 0]);
        assert!(apply_report(&mut s, &[3, 0b10, 0]));
        assert_eq!(s.buttons, 2);
        assert!(!apply_report(&mut s, &[3, 0b10, 0]), "unchanged report");
    }

    #[test]
    fn wireless_report_carries_all_six_axes() {
        let mut s = DeviceState::default();
        apply_report(&mut s, &[1, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
        assert_eq!(s.axes, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn short_and_unknown_reports_are_ignored() {
        let mut s = DeviceState::default();
        assert!(!apply_report(&mut s, &[]));
        assert!(!apply_report(&mut s, &[1, 2]));
        assert!(!apply_report(&mut s, &[0x17, 50]));
        assert_eq!(s, DeviceState::default());
    }

    #[test]
    fn hid_ids_are_parsed_and_matched() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:0000256F:0000C635\nHID_NAME=3Dconnexion SpaceMouse Compact\n";
        assert_eq!(parse_hid_id(uevent), Some((0x256f, 0xc635)));
        assert!(is_3d_mouse(0x256f, 0xc635));
        assert!(is_3d_mouse(0x046d, 0xc626));
        assert!(
            !is_3d_mouse(0x046d, 0xc52b),
            "a Logitech receiver is not a 3D mouse"
        );
    }
}
