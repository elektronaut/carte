//! The certificate stores carte's CA goes in on Linux: the system's, which curl,
//! OpenSSL and most programs read, and the NSS databases Chrome and Firefox read
//! instead. carte's entries are named `carte-<hash>`, after the start of the CA's
//! SHA-256 hash, so a replaced CA and uninstall find them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::prompt::tilde;
use crate::tls::CertificateAuthority;

use super::check::{Report, Status};
use super::{CaState, Facts, Plan, SetupEnv, Step};

/// Stands for the new CA's hash in a step planned before the CA exists; the step
/// gets the hash when it runs.
pub const NEW_CA_HASH: &str = "{new-ca-hash}";

/// How much of the CA's hash names its entries.
const HASH_LEN: usize = 16;

/// The name of the CA's entries in each store.
pub fn entry_name(hash: &str) -> String {
    format!("carte-{}", &hash[..hash.len().min(HASH_LEN)])
}

/// The system's store: a directory of anchors and the command that builds the bundle
/// from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemStore {
    pub dir: PathBuf,
    /// `update-ca-certificates` or `update-ca-trust`.
    pub update: String,
    /// `crt` or `pem`: what the update command reads.
    pub extension: String,
    /// carte's entries this install created, by hash prefix.
    pub own: Vec<String>,
    /// Other carte entries, which may be another user's.
    pub others: Vec<String>,
}

impl SystemStore {
    pub fn file(&self, hash: &str) -> PathBuf {
        self.dir
            .join(format!("{}.{}", entry_name(hash), self.extension))
    }
}

/// An NSS certificate database: `~/.pki/nssdb` for Chrome and Chromium, or a Firefox
/// profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NssDb {
    /// Who reads it, as in "Chrome and Chromium" or "Firefox (default-release)".
    pub name: String,
    pub dir: PathBuf,
    /// Whether the database exists; setup creates Chrome's, and leaves a Firefox
    /// profile that has none yet alone.
    pub exists: bool,
    pub own: Vec<String>,
    pub others: Vec<String>,
}

impl NssDb {
    fn sql(&self) -> String {
        format!("sql:{}", self.dir.display())
    }

    fn is_chrome(&self) -> bool {
        self.name == CHROME
    }
}

const CHROME: &str = "Chrome and Chromium";

/// Where the CA is trusted on Linux.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustFacts {
    pub system: Option<SystemStore>,
    /// Where `certutil` is, when it's installed.
    pub certutil: Option<String>,
    pub nss: Vec<NssDb>,
    /// Browsers from Snap or Flatpak, which keep certificates of their own.
    pub sandboxed: Vec<String>,
}

impl TrustFacts {
    pub fn gather(env: &SetupEnv) -> Self {
        let created: Vec<String> = CertificateAuthority::created_hashes(&env.paths.state_dir)
            .iter()
            .map(|hash| entry_name(hash))
            .collect();
        let on_path = |program: &str| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
            })
        };
        let system = system_store(&find_program, &|dir| Path::new(dir).is_dir());
        let system = system.map(|(dir, update, extension)| {
            let names = fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(&format!(".{extension}"))
                        .map(str::to_string)
                })
                .collect();
            let (own, others) = split_entries(names, &created);
            SystemStore {
                dir: dir.into(),
                update: update.to_string_lossy().into_owned(),
                extension: extension.into(),
                own,
                others,
            }
        });
        let certutil =
            find_user_program("certutil").map(|path| path.to_string_lossy().into_owned());
        let nss = nss_dbs(&env.home, &on_path)
            .into_iter()
            .map(|(name, dir)| {
                let exists = dir.join("cert9.db").is_file();
                let names = if let Some(certutil) = certutil.as_ref().filter(|_| exists) {
                    Command::new(certutil)
                        .arg("-L")
                        .arg("-d")
                        .arg(format!("sql:{}", dir.display()))
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .output()
                        .map(|o| parse_certutil_list(&String::from_utf8_lossy(&o.stdout)))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let (own, others) = split_entries(names, &created);
                NssDb {
                    name,
                    dir,
                    exists,
                    own,
                    others,
                }
            })
            .collect();
        let sandboxed = [
            ("snap/firefox/common/.mozilla/firefox", "Firefox from Snap"),
            (".var/app/org.mozilla.firefox", "Firefox from Flatpak"),
            ("snap/chromium", "Chromium from Snap"),
            (".var/app/org.chromium.Chromium", "Chromium from Flatpak"),
            (".var/app/com.google.Chrome", "Chrome from Flatpak"),
        ]
        .into_iter()
        .filter(|(dir, _)| env.home.join(dir).is_dir())
        .map(|(_, name)| name.to_string())
        .collect();
        TrustFacts {
            system,
            certutil,
            nss,
            sandboxed,
        }
    }
}

/// The system store's anchor directory, the program that rebuilds the bundle from it
/// and the extension it reads, for the first update program found whose directory
/// exists: Debian, Ubuntu and Alpine, openSUSE, then Fedora and Arch.
fn system_store(
    find: &dyn Fn(&str) -> Option<PathBuf>,
    is_dir: &dyn Fn(&str) -> bool,
) -> Option<(&'static str, PathBuf, &'static str)> {
    [
        (
            "update-ca-certificates",
            "/usr/local/share/ca-certificates",
            "crt",
        ),
        ("update-ca-certificates", "/etc/pki/trust/anchors", "pem"),
        ("update-ca-trust", "/etc/pki/ca-trust/source/anchors", "pem"),
        (
            "update-ca-trust",
            "/etc/ca-certificates/trust-source/anchors",
            "pem",
        ),
    ]
    .into_iter()
    .find_map(|(program, dir, extension)| {
        let update = find(program)?;
        is_dir(dir).then_some((dir, update, extension))
    })
}

/// Where the system's `program` is. Setup runs some through sudo, so only the
/// system's directories count, not the user's `PATH`; they include the sbin ones,
/// which a normal user's `PATH` often leaves out (Debian's does).
pub fn find_program(program: &str) -> Option<PathBuf> {
    find_in(
        program,
        [
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
        ]
        .map(PathBuf::from),
    )
}

/// Where a program setup runs as the user is: in the system's directories, then on
/// `PATH`, for one installed with Nix or Homebrew.
pub fn find_user_program(program: &str) -> Option<PathBuf> {
    find_program(program).or_else(|| {
        let path = std::env::var_os("PATH")?;
        find_in(
            program,
            std::env::split_paths(&path).filter(|dir| dir.is_absolute()),
        )
    })
}

fn find_in(program: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    dirs.into_iter().map(|dir| dir.join(program)).find(|path| {
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The NSS databases of the browsers found in `home`: Chrome's when its database or a
/// Chromium-based browser exists, and each Firefox profile.
fn nss_dbs(home: &Path, on_path: &dyn Fn(&str) -> bool) -> Vec<(String, PathBuf)> {
    let mut dbs = Vec::new();
    let chrome = home.join(".pki/nssdb");
    let chromium = [
        "google-chrome",
        "chromium",
        "BraveSoftware/Brave-Browser",
        "microsoft-edge",
    ]
    .iter()
    .any(|dir| home.join(".config").join(dir).is_dir())
        || [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ]
        .iter()
        .any(|program| on_path(program));
    if chrome.is_dir() || chromium {
        dbs.push((CHROME.to_string(), chrome));
    }
    for base in [".config/mozilla/firefox", ".mozilla/firefox"] {
        let base = home.join(base);
        let Ok(ini) = fs::read_to_string(base.join("profiles.ini")) else {
            continue;
        };
        for (name, dir) in parse_profiles_ini(&ini, &base) {
            if dir.is_dir() && !dbs.iter().any(|(_, d)| *d == dir) {
                dbs.push((format!("Firefox ({name})"), dir));
            }
        }
    }
    dbs
}

/// The profiles in Firefox's `profiles.ini`, by name and directory.
pub fn parse_profiles_ini(text: &str, base: &Path) -> Vec<(String, PathBuf)> {
    let mut profiles = Vec::new();
    let mut section = String::new();
    let (mut name, mut path, mut relative) = (None::<String>, None::<String>, true);
    let mut flush =
        |section: &str, name: &mut Option<String>, path: &mut Option<String>, relative: bool| {
            if section.starts_with("Profile")
                && let Some(dir) = path.take()
            {
                let dir = if relative {
                    base.join(&dir)
                } else {
                    PathBuf::from(&dir)
                };
                let name = name.take().unwrap_or_else(|| {
                    dir.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                profiles.push((name, dir));
            }
            *name = None;
            *path = None;
        };
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&section, &mut name, &mut path, relative);
            section = header.to_string();
            relative = true;
        } else if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => name = Some(value.trim().to_string()),
                "Path" => path = Some(value.trim().to_string()),
                "IsRelative" => relative = value.trim() != "0",
                _ => {}
            }
        }
    }
    flush(&section, &mut name, &mut path, relative);
    profiles
}

/// The carte nicknames in `certutil -L` output.
pub fn parse_certutil_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let (nickname, _trust) = line.rsplit_once(char::is_whitespace)?;
            Some(nickname.trim().to_string())
        })
        .filter(|nickname| nickname.starts_with("carte-"))
        .collect()
}

/// carte's entries split into this install's, by the names of the CAs it created,
/// and the others.
fn split_entries(names: Vec<String>, created: &[String]) -> (Vec<String>, Vec<String>) {
    names
        .into_iter()
        .filter(|name| {
            name.strip_prefix("carte-")
                .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .partition(|name| created.contains(name))
}

/// The steps that put the CA in every store and take out the ones it replaces.
pub(super) fn install_steps(env: &SetupEnv, facts: &Facts, trust: &TrustFacts, plan: &mut Plan) {
    let new_ca = matches!(facts.ca, CaState::Missing | CaState::Replace(_));
    let current = match (&facts.ca_hash, new_ca) {
        (Some(hash), false) => entry_name(hash),
        _ => format!("carte-{NEW_CA_HASH}"),
    };
    let ca_cert = env.ca_cert();
    let ca = ca_cert.to_string_lossy();
    let old = |own: &[String]| -> Vec<String> {
        own.iter()
            .filter(|name| **name != current)
            .cloned()
            .collect()
    };

    match &trust.system {
        Some(store) => {
            let file = store.dir.join(format!("{current}.{}", store.extension));
            let missing = !store.own.contains(&current);
            let old = old(&store.own);
            for name in &old {
                plan.steps.push(Step::sudo(
                    format!("Remove the old {name} from the system's certificates"),
                    "rm",
                    &[
                        "-f",
                        &store
                            .dir
                            .join(format!("{name}.{}", store.extension))
                            .to_string_lossy(),
                    ],
                ));
            }
            if missing {
                plan.steps.push(Step::sudo(
                    format!(
                        "Add carte's local CA to the system's certificates (it can only sign {} names)",
                        env.ca_names()
                    ),
                    "install",
                    &["-m", "644", &ca, &file.to_string_lossy()],
                ));
            }
            if missing || !old.is_empty() || facts.ca != CaState::Trusted {
                plan.steps.push(Step::sudo(
                    "Rebuild the system's certificate bundle",
                    &store.update,
                    &[],
                ));
            }
        }
        None => plan.notes.push(format!(
            "neither update-ca-certificates nor update-ca-trust is installed, so setup can't add \
             carte's CA to the system's certificates; add {} yourself",
            tilde(&ca_cert)
        )),
    }

    if let Some(certutil) = &trust.certutil {
        let before = plan.steps.len();
        for db in &trust.nss {
            if !db.exists {
                if !db.is_chrome() {
                    continue;
                }
                plan.steps.push(Step::run(
                    format!("Create the certificate database {} read", db.name),
                    "mkdir",
                    &["-p", &db.dir.to_string_lossy()],
                ));
                plan.steps.push(Step::run(
                    format!("Initialise {}", tilde(&db.dir)),
                    certutil,
                    &["-N", "--empty-password", "-d", &db.sql()],
                ));
            }
            if !db.own.contains(&current) {
                plan.steps.push(Step::run(
                    format!("Trust carte's local CA in {}", db.name),
                    certutil,
                    &[
                        "-A",
                        "-d",
                        &db.sql(),
                        "-t",
                        "C,,",
                        "-n",
                        &current,
                        "-i",
                        &ca,
                    ],
                ));
            }
            for name in old(&db.own) {
                plan.steps.push(Step::run(
                    format!("Remove the old {name} from {}", db.name),
                    certutil,
                    &["-D", "-d", &db.sql(), "-n", &name],
                ));
            }
        }
        if plan.steps.len() > before {
            plan.notes.push(
                "restart Chrome and Firefox so they read the certificates setup changed".into(),
            );
        }
    } else if !trust.nss.is_empty() {
        plan.notes.push(certutil_note(trust));
    }
    if !trust.sandboxed.is_empty() {
        plan.notes.push(sandboxed_note(env, trust));
    }
}

/// The steps that take carte's CAs out of every store.
pub(super) fn uninstall_steps(trust: &TrustFacts, plan: &mut Plan) {
    if let Some(store) = &trust.system {
        for name in &store.own {
            plan.steps.push(Step::sudo(
                format!("Remove {name} from the system's certificates"),
                "rm",
                &[
                    "-f",
                    &store
                        .dir
                        .join(format!("{name}.{}", store.extension))
                        .to_string_lossy(),
                ],
            ));
        }
        if !store.own.is_empty() {
            plan.steps.push(Step::sudo(
                "Rebuild the system's certificate bundle",
                &store.update,
                &[],
            ));
        }
    }
    match &trust.certutil {
        Some(certutil) => {
            for db in &trust.nss {
                for name in &db.own {
                    plan.steps.push(Step::run(
                        format!("Remove {name} from {}", db.name),
                        certutil,
                        &["-D", "-d", &db.sql(), "-n", name],
                    ));
                }
            }
        }
        None => {
            let dbs: Vec<&str> = trust
                .nss
                .iter()
                .filter(|db| db.exists)
                .map(|db| db.name.as_str())
                .collect();
            if !dbs.is_empty() {
                plan.notes.push(format!(
                    "certutil isn't installed, so entries named carte-<hash> may be left in the \
                     certificate databases of {}; remove them in the browsers' certificate \
                     settings, or install certutil and run `carte setup --uninstall` again",
                    dbs.join(", ")
                ));
            }
        }
    }
    let system = trust.system.iter().flat_map(|store| {
        store.others.iter().map(|name| {
            format!(
                "{} is left in the system's certificates; it isn't known to be this install's. \
                 If it is, remove it and run `sudo {}`.",
                store
                    .dir
                    .join(format!("{name}.{}", store.extension))
                    .display(),
                store.update
            )
        })
    });
    let nss = trust.nss.iter().flat_map(|db| {
        db.others.iter().map(|name| {
            format!(
                "{name} is left in {}; it isn't known to be this install's. If it is, remove it \
                 with `certutil -D -d {} -n {name}`.",
                db.name,
                db.sql()
            )
        })
    });
    plan.notes.extend(system.chain(nss).collect::<Vec<_>>());
}

fn certutil_note(trust: &TrustFacts) -> String {
    let names: Vec<&str> = trust.nss.iter().map(|db| db.name.as_str()).collect();
    format!(
        "{} read certificates from their own database, which needs certutil: install it \
         (libnss3-tools on Debian and Ubuntu, nss-tools on Fedora, nss on Arch) and run \
         `carte setup` again",
        names.join(", ")
    )
}

fn sandboxed_note(env: &SetupEnv, trust: &TrustFacts) -> String {
    format!(
        "{} keep certificates of their own; import {} as an authority in their settings to \
         trust carte's sites",
        trust.sandboxed.join(", "),
        tilde(&env.ca_cert())
    )
}

/// The stores' lines in `carte setup --check`.
pub(super) fn check(report: &mut Report, facts: &Facts, trust: &TrustFacts) {
    let current = facts.ca_hash.as_deref().map(entry_name);
    let has = |own: &[String]| current.as_ref().is_some_and(|c| own.contains(c));
    match &trust.system {
        Some(store) if has(&store.own) => report.push(
            Status::Ok,
            "system store",
            tilde(&store.file(facts.ca_hash.as_deref().unwrap_or(""))).to_string(),
        ),
        Some(store) => report.push(
            Status::Missing,
            "system store",
            format!("carte's CA isn't in {}", tilde(&store.dir)),
        ),
        None => report.push(
            Status::Warn,
            "system store",
            "neither update-ca-certificates nor update-ca-trust is installed",
        ),
    }
    for db in &trust.nss {
        let (status, detail) = if trust.certutil.is_none() {
            (
                Status::Warn,
                format!(
                    "{}: certutil isn't installed, so setup can't check it",
                    tilde(&db.dir)
                ),
            )
        } else if has(&db.own) {
            (Status::Ok, format!("{} trusts carte's CA", tilde(&db.dir)))
        } else if !db.exists && !db.is_chrome() {
            (
                Status::Info,
                format!("{} has no certificate database yet", tilde(&db.dir)),
            )
        } else {
            (
                Status::Missing,
                format!("{} doesn't trust carte's CA", tilde(&db.dir)),
            )
        };
        report.push(status, &db.name, detail);
    }
    for name in &trust.sandboxed {
        report.push(
            Status::Info,
            name,
            "keeps certificates of its own; setup doesn't change it",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::linux::LinuxFacts;
    use crate::setup::linux::tests::{HASH, bare, debian, on_linux, ready};
    use crate::setup::tests::{commands, env};
    use crate::setup::{Action, OsFacts, Platform, execute, plan_install, plan_uninstall};

    fn linux_env() -> SetupEnv {
        SetupEnv {
            platform: Platform::Linux,
            ..env(Path::new("/r"))
        }
    }

    fn db(name: &str, dir: &str, exists: bool, own: &[&str]) -> NssDb {
        NssDb {
            name: name.into(),
            dir: dir.into(),
            exists,
            own: own.iter().map(|n| n.to_string()).collect(),
            others: Vec::new(),
        }
    }

    fn with_trust(linux: LinuxFacts, trust: TrustFacts) -> Facts {
        Facts {
            ca: CaState::Trusted,
            ..on_linux(LinuxFacts { trust, ..linux })
        }
    }

    fn trust_of(facts: &Facts) -> &TrustFacts {
        match &facts.os {
            OsFacts::Linux(linux) => &linux.trust,
            OsFacts::MacOs(_) => panic!("not Linux facts"),
        }
    }

    #[test]
    fn trusts_the_ca_in_chrome_and_firefox() {
        let env = linux_env();
        let facts = with_trust(
            ready(&env),
            TrustFacts {
                system: Some(debian(&["carte-C544AB0012FF34EE"])),
                certutil: Some("/usr/bin/certutil".into()),
                nss: vec![
                    db(CHROME, "/r/home/.pki/nssdb", false, &[]),
                    db(
                        "Firefox (default-release)",
                        "/r/home/.mozilla/firefox/x.default-release",
                        true,
                        &["carte-1D2D000000000000"],
                    ),
                    db(
                        "Firefox (new)",
                        "/r/home/.mozilla/firefox/y.new",
                        false,
                        &[],
                    ),
                ],
                sandboxed: vec!["Firefox from Snap".into()],
            },
        );
        let plan = plan_install(&env, &facts);
        assert_eq!(
            commands(&plan),
            [
                "mkdir -p /r/home/.pki/nssdb",
                "/usr/bin/certutil -N --empty-password -d sql:/r/home/.pki/nssdb",
                "/usr/bin/certutil -A -d sql:/r/home/.pki/nssdb -t C,, -n carte-C544AB0012FF34EE -i /r/state/ca.pem",
                "/usr/bin/certutil -A -d sql:/r/home/.mozilla/firefox/x.default-release -t C,, -n carte-C544AB0012FF34EE -i /r/state/ca.pem",
                "/usr/bin/certutil -D -d sql:/r/home/.mozilla/firefox/x.default-release -n carte-1D2D000000000000",
                "verify",
            ]
        );
        assert!(!plan.steps.iter().any(Step::needs_sudo));
        assert_eq!(
            plan.notes[0],
            "restart Chrome and Firefox so they read the certificates setup changed"
        );
        assert!(
            plan.notes[1].starts_with("Firefox from Snap keep certificates of their own"),
            "{:?}",
            plan.notes
        );

        let mut report = Report::default();
        check(&mut report, &facts, trust_of(&facts));
        let shown = report.to_string();
        for expected in [
            "ok  system store               /usr/local/share/ca-certificates/carte-C544AB0012FF34EE.crt\n",
            "--  Chrome and Chromium        /r/home/.pki/nssdb doesn't trust carte's CA\n",
            "--  Firefox (default-release)  /r/home/.mozilla/firefox/x.default-release doesn't trust carte's CA\n",
            "    Firefox (new)              /r/home/.mozilla/firefox/y.new has no certificate database yet\n",
            "    Firefox from Snap          keeps certificates of its own; setup doesn't change it\n",
        ] {
            assert!(shown.contains(expected), "missing {expected:?} in\n{shown}");
        }
    }

    #[test]
    fn keeps_the_ca_it_restores_and_rebuilds_the_bundle() {
        let env = linux_env();
        let facts = Facts {
            ca: CaState::Restore,
            ..with_trust(
                ready(&env),
                TrustFacts {
                    system: Some(debian(&[
                        "carte-C544AB0012FF34EE",
                        "carte-1D2D000000000000",
                    ])),
                    certutil: Some("/usr/bin/certutil".into()),
                    ..TrustFacts::default()
                },
            )
        };
        assert_eq!(
            commands(&plan_install(&env, &facts)),
            [
                "create ca",
                "sudo rm -f /usr/local/share/ca-certificates/carte-1D2D000000000000.crt",
                "sudo /usr/sbin/update-ca-certificates",
                "verify",
            ]
        );
    }

    #[test]
    fn notes_what_it_cant_do() {
        let env = linux_env();
        let facts = with_trust(
            bare(),
            TrustFacts {
                system: None,
                certutil: None,
                nss: vec![db(CHROME, "/r/home/.pki/nssdb", true, &[])],
                sandboxed: Vec::new(),
            },
        );
        let plan = plan_install(&env, &facts);
        assert!(!commands(&plan).iter().any(|c| c.contains("certutil")));
        assert!(
            plan.notes[0].contains("neither update-ca-certificates nor update-ca-trust"),
            "{:?}",
            plan.notes
        );
        assert!(
            plan.notes[1].starts_with(
                "Chrome and Chromium read certificates from their own database, which needs \
                 certutil: install it (libnss3-tools"
            ),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn uninstalls_only_this_installs_entries() {
        let env = linux_env();
        let mut system = debian(&["carte-C544AB0012FF34EE"]);
        system.others = vec!["carte-7E7E000000000000".into()];
        let mut firefox = db(
            "Firefox (default)",
            "/r/home/.mozilla/firefox/x.default",
            true,
            &["carte-C544AB0012FF34EE"],
        );
        firefox.others = vec!["carte-7E7E000000000000".into()];
        let facts = with_trust(
            bare(),
            TrustFacts {
                system: Some(system),
                certutil: Some("/usr/bin/certutil".into()),
                nss: vec![firefox],
                sandboxed: Vec::new(),
            },
        );
        let plan = plan_uninstall(&env, &facts);
        assert_eq!(
            commands(&plan),
            [
                "sudo rm -f /usr/local/share/ca-certificates/carte-C544AB0012FF34EE.crt",
                "sudo /usr/sbin/update-ca-certificates",
                "/usr/bin/certutil -D -d sql:/r/home/.mozilla/firefox/x.default -n carte-C544AB0012FF34EE",
            ]
        );
        assert!(
            plan.notes.iter().any(|n| n.starts_with(
                "/usr/local/share/ca-certificates/carte-7E7E000000000000.crt is left in the \
                 system's certificates"
            )),
            "{:?}",
            plan.notes
        );
        assert!(
            plan.notes.iter().any(|n| n.contains(
                "`certutil -D -d sql:/r/home/.mozilla/firefox/x.default -n carte-7E7E000000000000`"
            )),
            "{:?}",
            plan.notes
        );

        let without_certutil = with_trust(
            bare(),
            TrustFacts {
                system: None,
                certutil: None,
                nss: vec![db(CHROME, "/r/home/.pki/nssdb", true, &[])],
                sandboxed: Vec::new(),
            },
        );
        let plan = plan_uninstall(&env, &without_certutil);
        assert!(
            plan.notes.iter().any(|n| n.starts_with(
                "certutil isn't installed, so entries named carte-<hash> may be left in the \
                 certificate databases of Chrome and Chromium"
            )),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn names_entries_by_hash_and_splits_them_by_owner() {
        assert_eq!(entry_name(HASH), "carte-C544AB0012FF34EE");
        assert_eq!(entry_name("1D2D"), "carte-1D2D");
        let (own, others) = split_entries(
            vec![
                "carte-C544AB0012FF34EE".into(),
                "carte-7E7E000000000000".into(),
                "carte-notes".into(),
                "other".into(),
            ],
            &["carte-C544AB0012FF34EE".into()],
        );
        assert_eq!(own, ["carte-C544AB0012FF34EE"]);
        assert_eq!(others, ["carte-7E7E000000000000"]);
    }

    #[test]
    fn finds_executables_in_the_given_directories() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (bin, sbin) = (dir.path().join("bin"), dir.path().join("sbin"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(sbin.join("update-ca-trust")).unwrap();
        let executable = |path: &Path, mode: u32| {
            fs::write(path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        executable(&sbin.join("update-ca-certificates"), 0o755);
        executable(&bin.join("certutil"), 0o644);
        let dirs = || [bin.clone(), sbin.clone()];
        assert_eq!(
            find_in("update-ca-certificates", dirs()),
            Some(sbin.join("update-ca-certificates"))
        );
        assert_eq!(find_in("update-ca-trust", dirs()), None);
        assert_eq!(find_in("certutil", dirs()), None);
        executable(&bin.join("update-ca-certificates"), 0o700);
        assert_eq!(
            find_in("update-ca-certificates", dirs()),
            Some(bin.join("update-ca-certificates"))
        );
    }

    #[test]
    fn picks_the_system_store_whose_directory_exists() {
        let store = |programs: &[&'static str], dirs: &[&'static str]| {
            let found = |p: &str| {
                programs
                    .contains(&p)
                    .then(|| Path::new("/usr/sbin").join(p))
            };
            let is_dir = |d: &str| dirs.contains(&d);
            system_store(&found, &is_dir)
                .map(|(dir, update, ext)| (dir, update.to_string_lossy().into_owned(), ext))
        };
        assert_eq!(
            store(
                &["update-ca-certificates"],
                &["/usr/local/share/ca-certificates"]
            ),
            Some((
                "/usr/local/share/ca-certificates",
                "/usr/sbin/update-ca-certificates".into(),
                "crt"
            ))
        );
        assert_eq!(
            store(&["update-ca-certificates"], &["/etc/pki/trust/anchors"]),
            Some((
                "/etc/pki/trust/anchors",
                "/usr/sbin/update-ca-certificates".into(),
                "pem"
            ))
        );
        assert_eq!(
            store(
                &["update-ca-trust"],
                &["/etc/ca-certificates/trust-source/anchors"]
            ),
            Some((
                "/etc/ca-certificates/trust-source/anchors",
                "/usr/sbin/update-ca-trust".into(),
                "pem"
            ))
        );
        assert_eq!(store(&["update-ca-certificates"], &[]), None);
        assert_eq!(store(&[], &["/usr/local/share/ca-certificates"]), None);
    }

    #[test]
    fn reads_certutil_listings() {
        let listing = "\nCertificate Nickname                                         Trust Attributes\n\
                       \x20                                                            SSL,S/MIME,JAR/XPI\n\n\
                       carte-C544AB0012FF34EE                                       C,,\n\
                       Some Other CA                                                CT,C,C\n";
        assert_eq!(parse_certutil_list(listing), ["carte-C544AB0012FF34EE"]);
    }

    #[test]
    fn reads_firefox_profiles() {
        let ini = "[Install4F96D1932A9F858E]\nDefault=x.default-release\n\n\
                   [Profile1]\nName=default\nIsRelative=1\nPath=y.default\n\n\
                   [Profile0]\nName=default-release\nIsRelative=1\nPath=x.default-release\nDefault=1\n\n\
                   [Profile2]\nName=elsewhere\nIsRelative=0\nPath=/data/ff\n\n\
                   [General]\nStartWithLastProfile=1\nVersion=2\n";
        let base = Path::new("/home/me/.config/mozilla/firefox");
        assert_eq!(
            parse_profiles_ini(ini, base),
            [
                ("default".to_string(), base.join("y.default")),
                (
                    "default-release".to_string(),
                    base.join("x.default-release")
                ),
                ("elsewhere".to_string(), PathBuf::from("/data/ff")),
            ]
        );
    }

    #[test]
    fn fills_in_the_new_cas_hash_when_the_step_runs() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let target = dir.path().join("trusted");
        fs::create_dir_all(&target).unwrap();
        let copy = target.join(format!("carte-{NEW_CA_HASH}.crt"));
        let steps = [
            Step {
                description: "ca".into(),
                action: Action::CreateCa,
            },
            Step::run(
                "copy",
                "cp",
                &[&env.ca_cert().to_string_lossy(), &copy.to_string_lossy()],
            ),
        ];
        execute(&steps, &env).unwrap();
        let hash = crate::tls::ca::current_hash(&env.paths.state_dir).unwrap();
        assert!(target.join(format!("{}.crt", entry_name(&hash))).is_file());
    }
}
