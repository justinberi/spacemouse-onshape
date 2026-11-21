//! One-time setup for a `cargo install`: the udev rule, browser trust for the certificate, the
//! browser extension's files, and a systemd user service that starts the bridge at login.
//! (The .deb package does the same through its own files and scripts.)

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::certs;

pub const UDEV_RULE: &str = include_str!("../packaging/70-spacemouse.rules");
const UDEV_RULE_NAME: &str = "70-spacemouse.rules";
const UDEV_DIRS: &[&str] = &[
    "/etc/udev/rules.d",
    "/usr/lib/udev/rules.d",
    "/lib/udev/rules.d",
];

const SERVICE_NAME: &str = "spacemouse-onshape.service";

/// The unpacked browser extension, written next to the certificate so it has a stable path.
const EXTENSION_FILES: &[(&str, &str)] = &[
    ("manifest.json", include_str!("../extension/manifest.json")),
    ("platform.js", include_str!("../extension/platform.js")),
];

/// Writes the unpacked extension into `data_dir/extension` and returns that folder.
pub fn write_extension(data_dir: &Path) -> Result<PathBuf> {
    let dir = data_dir.join("extension");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in EXTENSION_FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*contents) {
            std::fs::write(&path, contents)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(dir)
}

fn udev_rule_installed() -> Option<PathBuf> {
    UDEV_DIRS
        .iter()
        .map(|d| Path::new(d).join(UDEV_RULE_NAME))
        .find(|p| p.exists())
}

fn sudo(args: &[&str]) -> Result<()> {
    let status = Command::new("sudo")
        .args(args)
        .status()
        .context("running sudo")?;
    if !status.success() {
        bail!("sudo {} failed", args.join(" "));
    }
    Ok(())
}

fn systemctl_user(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl")?;
    if !status.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

fn user_unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("systemd/user").join(SERVICE_NAME))
}

/// Whether the .deb's unit (in /usr/lib/systemd/user) is what systemd will run.
fn packaged_unit() -> bool {
    Path::new("/usr/lib/systemd/user")
        .join(SERVICE_NAME)
        .exists()
}

fn step(n: usize, text: &str) {
    println!("\n[{n}/4] {text}");
}

pub fn setup(paths: &certs::Paths, start_service: bool) -> Result<()> {
    step(1, "Device access (udev rule)");
    match udev_rule_installed() {
        Some(p) => println!("      already installed: {}", p.display()),
        None => {
            println!(
                "      installing /etc/udev/rules.d/{UDEV_RULE_NAME}; sudo will ask for your password"
            );
            let tmp = paths.dir.join(UDEV_RULE_NAME);
            std::fs::create_dir_all(&paths.dir)?;
            std::fs::write(&tmp, UDEV_RULE)?;
            let target = format!("/etc/udev/rules.d/{UDEV_RULE_NAME}");
            sudo(&["install", "-m", "644", &tmp.to_string_lossy(), &target])?;
            let _ = std::fs::remove_file(&tmp);
            sudo(&["udevadm", "control", "--reload"])?;
            sudo(&[
                "udevadm",
                "trigger",
                "--subsystem-match=hidraw",
                "--action=change",
            ])?;
            println!(
                "      done (replug the device if `spacemouse-onshape monitor` can't read it)"
            );
        }
    }

    step(2, "Browser trust for the bridge's certificate");
    certs::load_or_create(paths)?;
    match certs::install_ca(paths) {
        Ok(changed) if changed.is_empty() => println!("      already trusted"),
        Ok(changed) => {
            for db in changed {
                println!("      added to {}", db.display());
            }
            println!("      restart your browser to pick it up");
        }
        Err(e) => println!(
            "      skipped: {e:#}\n      (or open https://{}:8181 and accept the warning)",
            certs::NLPROXY_IP
        ),
    }

    step(3, "Start at login (systemd user service)");
    if packaged_unit() {
        println!("      provided by the installed package");
    } else {
        let exe = std::env::current_exe().context("finding this program's path")?;
        let unit = user_unit_path()?;
        std::fs::create_dir_all(unit.parent().expect("unit path has a parent"))?;
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=3Dconnexion SpaceMouse bridge for Onshape\n\n[Service]\n\
                 ExecStart={} serve\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )?;
        println!("      wrote {}", unit.display());
        systemctl_user(&["daemon-reload"])?;
    }
    if start_service {
        systemctl_user(&["enable", SERVICE_NAME])?;
        systemctl_user(&["restart", SERVICE_NAME])?;
        println!("      enabled and started");
    }

    step(4, "Browser extension (once per browser)");
    let ext = write_extension(&paths.dir)?;
    println!(
        "      Onshape only looks for a 3D mouse on Windows and macOS; the extension fixes that.\n\
         \n      Chrome/Chromium/Brave/Edge: open chrome://extensions, turn on Developer mode,\n\
         \x20     choose \"Load unpacked\" and select:\n\n          {}\n\
         \n      Firefox, or with Tampermonkey/Violentmonkey: install the userscript from\n\
         \x20     https://{}:8181/onshape-spacemouse.user.js",
        ext.display(),
        certs::NLPROXY_IP
    );
    println!(
        "\nThen open an Onshape document and move the cap. Status: https://{}:8181",
        certs::NLPROXY_IP
    );
    Ok(())
}

pub fn uninstall(paths: &certs::Paths) -> Result<()> {
    let _ = systemctl_user(&["disable", "--now", SERVICE_NAME]);
    let unit = user_unit_path()?;
    if unit.exists() {
        std::fs::remove_file(&unit)?;
        let _ = systemctl_user(&["daemon-reload"]);
        println!("removed {}", unit.display());
    }
    for db in certs::uninstall_ca()? {
        println!("removed the CA from {}", db.display());
    }
    if paths.dir.exists() {
        std::fs::remove_dir_all(&paths.dir)?;
        println!("removed {}", paths.dir.display());
    }
    if let Some(rule) = udev_rule_installed() {
        println!(
            "kept the udev rule {} (other programs such as SolidCraft use it); remove it with:\n  sudo rm {}",
            rule.display(),
            rule.display()
        );
    }
    println!("remove the browser extension from chrome://extensions");
    Ok(())
}
