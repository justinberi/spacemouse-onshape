//! spacemouse-onshape: use a 3Dconnexion SpaceMouse with Onshape (and other browser apps built on
//! 3Dconnexion's navigation library) on Linux.
//!
//! On Windows and macOS, 3Dconnexion's driver runs a local HTTPS/WebSocket server ("Nl-Proxy")
//! at `127.51.68.120:8181`, which the JavaScript inside Onshape talks to. This program is that
//! server for Linux, reading the device directly through `hidraw`.

mod certs;
mod device;
mod nav;
mod server;
mod session;
mod setup;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::watch;

use crate::device::DeviceState;
use crate::session::ButtonAction;

#[derive(Parser)]
#[command(version, about, long_about = None, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Options for running the bridge without naming the `serve` command.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bridge (the default).
    Serve(ServeArgs),
    /// Print the device's motion, to check that it is found and which way its axes go.
    Monitor(DeviceArgs),
    /// Show where the certificate authority is, or add it to Chrome's and Firefox's stores.
    Cert {
        /// Add the CA with `certutil` to ~/.pki/nssdb (Chrome, Chromium) and Firefox profiles.
        #[arg(long)]
        install: bool,
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Print the userscript that lets Onshape look for the bridge.
    Userscript,
    /// One-time setup: device access, certificate trust, start at login, browser extension.
    Setup {
        /// Set up, but don't enable or start the login service.
        #[arg(long)]
        no_service: bool,
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Undo `setup`: stop the service, remove it, untrust and delete the certificate.
    Uninstall {
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
}

#[derive(clap::Args, Clone)]
struct DeviceArgs {
    /// Read this hidraw node instead of searching, e.g. /dev/hidraw7.
    #[arg(long, value_name = "PATH")]
    device: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    /// The model follows the cap.
    Object,
    /// The camera follows the cap.
    Camera,
}

#[derive(clap::Args, Clone)]
struct ServeArgs {
    #[command(flatten)]
    device: DeviceArgs,

    /// Address to listen on. 3Dconnexion's library connects to 127.51.68.120.
    #[arg(long, default_value = "127.51.68.120:8181")]
    listen: SocketAddr,

    /// Whether the cap moves the model or the camera.
    #[arg(long, value_enum, default_value = "object")]
    mode: ModeArg,

    /// Multiplies every speed below.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Pan speed: view heights per second at full deflection.
    #[arg(long, default_value_t = nav::Settings::default().pan_speed)]
    pan_speed: f64,
    /// Rotation speed: radians per second at full deflection.
    #[arg(long, default_value_t = nav::Settings::default().rotate_speed)]
    rotate_speed: f64,
    /// Zoom speed: e-folds per second at full deflection.
    #[arg(long, default_value_t = nav::Settings::default().zoom_speed)]
    zoom_speed: f64,
    /// Fraction of full deflection ignored around rest (0 to 0.5).
    #[arg(long, default_value_t = nav::Settings::default().deadzone)]
    deadzone: f64,
    /// Response curve exponent: 1 is linear, higher is finer near rest.
    #[arg(long, default_value_t = nav::Settings::default().curve)]
    curve: f64,
    /// Axes to invert (comma separated): pan-x, pan-y, zoom, pitch, yaw, roll.
    #[arg(long, value_delimiter = ',', value_name = "AXES")]
    invert: Vec<String>,
    /// Turn off rotation.
    #[arg(long)]
    no_rotate: bool,
    /// Turn off pan and zoom.
    #[arg(long)]
    no_translate: bool,

    /// What each button does, in order (comma separated): fit, front, back, left, right, top,
    /// bottom, iso or none.
    #[arg(long, value_delimiter = ',', default_value = "front,fit")]
    buttons: Vec<ButtonAction>,

    /// Camera updates per second while the cap moves.
    #[arg(long, default_value_t = 60)]
    frame_rate: u32,

    /// Also accept pages from this origin (e.g. https://example.com). Onshape's and
    /// 3Dconnexion's sites are always accepted.
    #[arg(long, value_name = "ORIGIN")]
    allow_origin: Vec<String>,
    /// Accept pages from any origin.
    #[arg(long)]
    allow_any_origin: bool,

    /// At startup, add the certificate authority to browsers that don't trust it yet (needs
    /// certutil). The packaged service uses this.
    #[arg(long)]
    trust_browsers: bool,

    /// Where the certificate is kept [default: ~/.local/share/spacemouse-onshape].
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
}

impl ServeArgs {
    fn nav_settings(&self) -> Result<nav::Settings> {
        let mut invert = [false; 6];
        for name in &self.invert {
            let name = name.trim();
            let i = nav::AXIS_NAMES
                .iter()
                .position(|n| *n == name)
                .with_context(|| {
                    format!("unknown axis {name:?}; use {}", nav::AXIS_NAMES.join(", "))
                })?;
            invert[i] = true;
        }
        if !(0.0..=0.5).contains(&self.deadzone) {
            bail!("--deadzone must be between 0 and 0.5");
        }
        if !(self.curve > 0.0 && self.curve <= 5.0) {
            bail!("--curve must be between 0 and 5");
        }
        Ok(nav::Settings {
            mode: match self.mode {
                ModeArg::Object => nav::Mode::Object,
                ModeArg::Camera => nav::Mode::Camera,
            },
            pan_speed: self.pan_speed * self.speed,
            rotate_speed: self.rotate_speed * self.speed,
            zoom_speed: self.zoom_speed * self.speed,
            deadzone: self.deadzone,
            curve: self.curve,
            invert,
            no_rotate: self.no_rotate,
            no_translate: self.no_translate,
        })
    }
}

fn data_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    arg.map_or_else(certs::Paths::default_dir, Ok)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "spacemouse_onshape=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve(cli.serve)) {
        Command::Serve(args) => serve(args),
        Command::Monitor(args) => monitor(args),
        Command::Cert {
            install,
            data_dir: dir,
        } => {
            let paths = certs::Paths::new(data_dir(dir)?);
            certs::load_or_create(&paths)?;
            if install {
                for db in certs::install_ca(&paths)? {
                    println!("added the CA to {}", db.display());
                }
                println!("restart the browser to pick it up");
            } else {
                println!("{}", paths.ca.display());
            }
            Ok(())
        }
        Command::Setup {
            no_service,
            data_dir: dir,
        } => setup::setup(&certs::Paths::new(data_dir(dir)?), !no_service),
        Command::Uninstall { data_dir: dir } => {
            setup::uninstall(&certs::Paths::new(data_dir(dir)?))
        }
        Command::Userscript => {
            print!("{}", server::USERSCRIPT);
            Ok(())
        }
    }
}

fn serve(args: ServeArgs) -> Result<()> {
    let settings = args.nav_settings()?;
    if !args.listen.ip().is_loopback() {
        bail!(
            "--listen must be a loopback address (127.x.x.x); the bridge is for this computer only"
        );
    }
    if args.frame_rate == 0 || args.frame_rate > 500 {
        bail!("--frame-rate must be between 1 and 500");
    }
    let paths = certs::Paths::new(data_dir(args.data_dir.clone())?);
    let tls = certs::load_or_create(&paths)?;
    if args.trust_browsers {
        match certs::install_ca(&paths) {
            Ok(changed) => {
                for db in changed {
                    tracing::info!(
                        "browsers using {} now trust the certificate (restart them)",
                        db.display()
                    );
                }
            }
            Err(e) => tracing::warn!("cannot add the certificate to browsers: {e:#}"),
        }
    }
    match setup::write_extension(&paths.dir) {
        Ok(dir) => tracing::info!("browser extension (load unpacked): {}", dir.display()),
        Err(e) => tracing::warn!("cannot write the browser extension: {e:#}"),
    }

    let (tx, rx) = watch::channel(DeviceState::default());
    device::spawn_reader(tx, args.device.device.clone());

    let config = session::Config {
        nav: settings,
        frame_interval: Duration::from_secs_f64(1.0 / f64::from(args.frame_rate)),
        buttons: args.buttons.clone(),
    };
    let app = server::App::new(
        config,
        rx,
        args.listen.port(),
        args.allow_origin.clone(),
        args.allow_any_origin,
    );

    let url = match args.listen.ip() {
        IpAddr::V4(ip) => format!("https://{ip}:{}", args.listen.port()),
        IpAddr::V6(ip) => format!("https://[{ip}]:{}", args.listen.port()),
    };
    tracing::info!("listening on {url}");
    tracing::info!("open {url} once in the browser to check the certificate and the device");
    tracing::info!(
        "certificate authority: {} (see `spacemouse-onshape cert --help`)",
        paths.ca.display()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        tokio::select! {
            r = server::serve(app, args.listen, tls) => r.with_context(|| format!("serving on {}", args.listen)),
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}

fn monitor(args: DeviceArgs) -> Result<()> {
    let (tx, mut rx) = watch::channel(DeviceState::default());
    device::spawn_reader(tx, args.device);
    let settings = nav::Settings::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut was_connected = None;
        while rx.changed().await.is_ok() {
            let s = rx.borrow_and_update().clone();
            if was_connected != Some(s.connected) {
                match (&s.name, &s.problem) {
                    (Some(name), _) if s.connected => println!("connected: {name}"),
                    (_, Some(p)) => println!("not connected: {p}"),
                    _ => println!("not connected"),
                }
                was_connected = Some(s.connected);
            }
            if s.connected {
                let i = nav::input_from_raw(s.axes, &settings);
                println!(
                    "raw {:>5} {:>5} {:>5} {:>5} {:>5} {:>5}  buttons {:#04b}  |  pan-x {:+.2} pan-y {:+.2} zoom {:+.2} pitch {:+.2} yaw {:+.2} roll {:+.2}",
                    s.axes[0], s.axes[1], s.axes[2], s.axes[3], s.axes[4], s.axes[5], s.buttons,
                    i.t.x, i.t.y, i.t.z, i.w.x, i.w.y, i.w.z,
                );
            }
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
