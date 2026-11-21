//! The TLS certificate for `https://127.51.68.120:8181`.
//!
//! Onshape's page is served over HTTPS, so it may only talk to the bridge over HTTPS/WSS, and the
//! browser must trust the bridge's certificate. On first run a local certificate authority is
//! made and used to sign a server certificate, and then the CA's private key is thrown away. The
//! CA is also name-constrained to loopback addresses and `localhost`. Trusting it in a browser
//! therefore lets nobody, including this program, issue certificates for any real site.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The address 3Dconnexion's navigation library connects to.
pub const NLPROXY_IP: Ipv4Addr = Ipv4Addr::new(127, 51, 68, 120);

pub const CA_NAME: &str = "spacemouse-onshape local CA";

pub struct Paths {
    pub dir: PathBuf,
    /// The CA certificate (PEM) to import into a browser.
    pub ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

impl Paths {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            ca: dir.join("ca.crt"),
            cert: dir.join("server.der"),
            key: dir.join("server.key.der"),
            dir,
        }
    }

    /// `$XDG_DATA_HOME/spacemouse-onshape`, or `~/.local/share/spacemouse-onshape`.
    pub fn default_dir() -> Result<PathBuf> {
        if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(d).join("spacemouse-onshape"));
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".local/share/spacemouse-onshape"))
    }
}

/// Loads the server certificate, making one (and its CA) first if there is none.
pub fn load_or_create(paths: &Paths) -> Result<rustls::ServerConfig> {
    if !(paths.cert.exists() && paths.key.exists() && paths.ca.exists()) {
        create(paths)?;
        tracing::info!("made a new certificate authority: {}", paths.ca.display());
    }
    let cert =
        std::fs::read(&paths.cert).with_context(|| format!("reading {}", paths.cert.display()))?;
    let key =
        std::fs::read(&paths.key).with_context(|| format!("reading {}", paths.key.display()))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )
    .context("loading the server certificate")?;
    // HTTP/1.1 only: browsers then open WebSockets with a plain Upgrade.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn create(paths: &Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;

    let mut ca = CertificateParams::default();
    ca.distinguished_name = DistinguishedName::new();
    ca.distinguished_name.push(DnType::CommonName, CA_NAME);
    ca.distinguished_name
        .push(DnType::OrganizationName, "spacemouse-onshape");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([127, 0, 0, 0], 8)),
            GeneralSubtree::DnsName("localhost".into()),
        ],
        excluded_subtrees: vec![],
    });
    let year = start_year();
    ca.not_before = rcgen::date_time_ymd(year, 1, 1);
    ca.not_after = rcgen::date_time_ymd(year + 20, 1, 1);
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca.self_signed(&ca_key)?;

    let mut server = CertificateParams::new(vec!["localhost".to_string()])?;
    server.distinguished_name = DistinguishedName::new();
    server
        .distinguished_name
        .push(DnType::CommonName, NLPROXY_IP.to_string());
    server
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(NLPROXY_IP)));
    server
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    server.not_before = rcgen::date_time_ymd(year, 1, 1);
    server.not_after = rcgen::date_time_ymd(year + 20, 1, 1);
    let server_key = KeyPair::generate()?;
    let issuer = Issuer::new(ca, ca_key);
    let server_cert = server.signed_by(&server_key, &issuer)?;
    // `issuer` (and with it the CA's private key) is dropped here and never written anywhere.
    drop(issuer);

    write_private(&paths.key, &server_key.serialize_der())?;
    std::fs::write(&paths.cert, server_cert.der())?;
    std::fs::write(&paths.ca, ca_cert.pem())?;
    Ok(())
}

/// Last year, so a certificate made now is valid at once whatever the clock's time zone.
fn start_year() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    // Approximate is fine: it only has to be in the past.
    1970 + (secs / 31_556_952) as i32 - 1
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

/// The NSS certificate databases browsers read: Chrome/Chromium's `~/.pki/nssdb` (made if
/// missing) and every Firefox profile found (regular, snap and Flatpak installs).
fn nss_databases() -> Result<Vec<PathBuf>> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let mut dbs = vec![home.join(".pki/nssdb")];
    for profiles in [
        home.join(".mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ] {
        if let Ok(entries) = std::fs::read_dir(profiles) {
            dbs.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.join("cert9.db").exists()),
            );
        }
    }
    Ok(dbs)
}

fn require_certutil() -> Result<()> {
    which("certutil").map(|_| ()).context(
        "certutil not found; install it (Debian/Ubuntu: sudo apt install libnss3-tools, Fedora: \
         nss-tools, Arch: nss) or import the CA in the browser's certificate settings",
    )
}

fn has_ca(spec: &str) -> bool {
    std::process::Command::new("certutil")
        .args(["-L", "-d", spec, "-n", CA_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Makes browsers trust the CA: adds it, with `certutil`, to every NSS database that doesn't have
/// it yet. Returns the databases changed; an empty list means all already trusted it.
pub fn install_ca(paths: &Paths) -> Result<Vec<PathBuf>> {
    require_certutil()?;
    let ca = paths.ca.to_string_lossy();
    let mut changed = Vec::new();
    for db in nss_databases()? {
        let spec = format!("sql:{}", db.display());
        if !db.join("cert9.db").exists() {
            std::fs::create_dir_all(&db)?;
            run("certutil", &["-N", "-d", &spec, "--empty-password"])?;
        }
        if has_ca(&spec) {
            // A CA from an earlier install has the same name but another key: replace it.
            let current = std::process::Command::new("certutil")
                .args(["-L", "-d", &spec, "-n", CA_NAME, "-a"])
                .output()?;
            let ours = std::fs::read_to_string(&paths.ca)?;
            if normalize_pem(&String::from_utf8_lossy(&current.stdout)) == normalize_pem(&ours) {
                continue;
            }
            run("certutil", &["-D", "-d", &spec, "-n", CA_NAME])?;
        }
        run(
            "certutil",
            &["-A", "-d", &spec, "-t", "C,,", "-n", CA_NAME, "-i", &ca],
        )?;
        changed.push(db);
    }
    Ok(changed)
}

/// Removes the CA from every NSS database that has it. Returns the databases changed.
pub fn uninstall_ca() -> Result<Vec<PathBuf>> {
    if which("certutil").is_none() {
        return Ok(Vec::new());
    }
    let mut changed = Vec::new();
    for db in nss_databases()? {
        let spec = format!("sql:{}", db.display());
        while db.join("cert9.db").exists() && has_ca(&spec) {
            run("certutil", &["-D", "-d", &spec, "-n", CA_NAME])?;
            if !changed.contains(&db) {
                changed.push(db.clone());
            }
        }
    }
    Ok(changed)
}

fn normalize_pem(pem: &str) -> String {
    pem.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(program))
        .find(|p| p.is_file())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program).args(args).status()?;
    anyhow::ensure!(status.success(), "{program} {} failed", args.join(" "));
    Ok(())
}
