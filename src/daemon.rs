//! The clock. `run` ticks in a loop; `install` makes the OS start it at
//! login and keep it alive. `tick` itself lives in `task`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::StoreError;
use crate::store::Store;
use crate::task;

fn data_version(store: &Store) -> Result<i64, StoreError> {
    Ok(store.connection().query_row("PRAGMA data_version", [], |r| r.get(0))?)
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Tick now, then whenever `interval` has passed or another connection
/// wrote to the database (checked every `poll`). Returns on ctrl-c or
/// SIGTERM.
pub async fn run(store: &mut Store, db: &Path, interval: Duration, poll: Duration) -> Result<(), StoreError> {
    let mut stop = Box::pin(shutdown());
    tracing::info!(db = %db.display(), interval = ?interval, poll = ?poll, "daemon started");
    loop {
        let now = store.now()?;
        match task::tick(store, db, &now).await {
            Ok(report) => {
                if !report.fired.is_empty() || !report.errors.is_empty() {
                    tracing::info!(fired = ?report.fired, errors = ?report.errors, "tick");
                }
            }
            Err(e) => tracing::error!(error = %e, "tick failed"),
        }
        let last_tick = Instant::now();
        // Our own writes do not move data_version; other connections' do.
        let version = data_version(store)?;
        loop {
            tokio::select! {
                _ = &mut stop => {
                    tracing::info!("daemon stopping");
                    return Ok(());
                }
                _ = tokio::time::sleep(poll) => {}
            }
            if last_tick.elapsed() >= interval || data_version(store)? != version {
                break;
            }
        }
    }
}

// ---- install ------------------------------------------------------------

/// The name of a vault's service on every platform: `io.dreams.vault-<id>`.
/// It is the launchd label, the systemd unit without `.service`, and the stem
/// of the service and log files. The id never replicates, so two vaults never
/// share a name, and a vault that moves keeps its name.
pub fn service_name(vault_id: &str) -> String {
    format!("io.dreams.vault-{vault_id}")
}

const PLIST: &str = ".plist";
const UNIT: &str = ".service";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A launchd user agent that keeps the daemon running.
pub fn render_plist(label: &str, exe: &Path, db: &Path, log: &Path, path_env: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--db</string>
    <string>{db}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&exe.to_string_lossy()),
        db = xml_escape(&db.to_string_lossy()),
        path = xml_escape(path_env),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// A systemd user service that keeps the daemon running.
pub fn render_unit(exe: &Path, db: &Path, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=dreams daemon for {db}\n\n[Service]\nExecStart={exe} --db {db} daemon\nRestart=always\nRestartSec=5\nEnvironment=PATH={path}\n\n[Install]\nWantedBy=default.target\n",
        exe = exe.to_string_lossy(),
        db = db.to_string_lossy(),
        path = path_env,
    )
}

/// The part of `render_plist` that names `db`.
fn plist_fragment(db: &Path) -> String {
    format!("<string>--db</string>\n    <string>{}</string>", xml_escape(&db.to_string_lossy()))
}

/// The part of `render_unit` that names `db`.
fn unit_fragment(db: &Path) -> String {
    format!(" --db {} daemon\n", db.to_string_lossy())
}

/// The names of the services in `dir` whose file is `<name><ext>` and
/// contains `fragment`. Any name matches, so older names are found too. A
/// missing `dir` has none.
fn services_for(dir: &Path, ext: &str, fragment: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let name = file.strip_suffix(ext)?.to_string();
            std::fs::read_to_string(e.path()).ok()?.contains(fragment).then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// The services of a vault: every one that points at its database, and
/// the one its id names, if that is installed. Old names and the service of
/// a deleted vault are found by the path.
fn installed_for(dir: &Path, ext: &str, fragment: &str, vault_id: Option<&str>) -> Vec<String> {
    let mut names = services_for(dir, ext, fragment);
    if let Some(name) = vault_id.map(service_name)
        && !names.contains(&name)
        && dir.join(format!("{name}{ext}")).exists()
    {
        names.push(name);
    }
    names
}

fn home() -> Result<PathBuf, StoreError> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| StoreError::invalid("HOME is not set"))
}

fn agents_dir(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
}

fn units_dir(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
}

fn sh(program: &str, args: &[String]) -> Result<(), StoreError> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| StoreError::invalid(format!("running {program}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(StoreError::invalid(format!("{program} {} failed with {status}", args.join(" "))))
    }
}

#[cfg(unix)]
fn uid(home: &Path) -> Result<u32, StoreError> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(home).map_err(|e| StoreError::invalid(format!("{}: {e}", home.display())))?.uid())
}

fn io(e: std::io::Error) -> StoreError {
    StoreError::invalid(e.to_string())
}

fn remove_file(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
        _ => Ok(()),
    }
}

/// Stop the launchd agent `name` (its label) and remove its plist.
fn remove_agent(home: &Path, name: &str) -> Result<(), StoreError> {
    let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{name}", uid(home)?)]).output();
    remove_file(&agents_dir(home).join(format!("{name}{PLIST}")))
}

/// Stop the systemd user unit `name.service` and remove its file.
fn remove_unit(home: &Path, name: &str) -> Result<(), StoreError> {
    let unit = format!("{name}{UNIT}");
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit]).output();
    remove_file(&units_dir(home).join(&unit))?;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    Ok(())
}

/// Register the daemon of the vault at `db` with the OS, named by its id.
/// Other services for the same `db`, such as ones with an old name, are
/// removed first. `db` must be absolute.
pub fn install(db: &Path, vault_id: &str, exe: &Path, out: &mut dyn Write) -> Result<(), StoreError> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = home()?;
    let name = service_name(vault_id);
    if cfg!(target_os = "macos") {
        for old in services_for(&agents_dir(&home), PLIST, &plist_fragment(db)) {
            if old != name {
                remove_agent(&home, &old)?;
                writeln!(out, "removed {old}").map_err(io)?;
            }
        }
        let label = name;
        let plist = agents_dir(&home).join(format!("{label}{PLIST}"));
        let log = home.join("Library/Logs/dreams").join(format!("{label}.log"));
        std::fs::create_dir_all(plist.parent().unwrap()).map_err(io)?;
        std::fs::create_dir_all(log.parent().unwrap()).map_err(io)?;
        std::fs::write(&plist, render_plist(&label, exe, db, &log, &path_env)).map_err(io)?;
        let domain = format!("gui/{}", uid(&home)?);
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
        sh("launchctl", &["bootstrap".into(), domain, plist.to_string_lossy().into_owned()])?;
        writeln!(out, "installed {label}\n  {}\nlogs: {}", plist.display(), log.display()).map_err(io)?;
    } else if cfg!(target_os = "linux") {
        for old in services_for(&units_dir(&home), UNIT, &unit_fragment(db)) {
            if old != name {
                remove_unit(&home, &old)?;
                writeln!(out, "removed {old}{UNIT}").map_err(io)?;
            }
        }
        let unit_name = format!("{name}{UNIT}");
        let unit = units_dir(&home).join(&unit_name);
        std::fs::create_dir_all(unit.parent().unwrap()).map_err(io)?;
        std::fs::write(&unit, render_unit(exe, db, &path_env)).map_err(io)?;
        sh("systemctl", &["--user".into(), "daemon-reload".into()])?;
        sh("systemctl", &["--user".into(), "enable".into(), "--now".into(), unit_name.clone()])?;
        writeln!(
            out,
            "installed {unit_name}\n  {}\nlogs: journalctl --user -u {unit_name}\nTo keep it running while logged out: loginctl enable-linger",
            unit.display()
        )
        .map_err(io)?;
    } else {
        return Err(StoreError::invalid(format!(
            "no installer for this platform; run this yourself at login:\n  {} --db {} daemon",
            exe.display(),
            db.display()
        )));
    }
    Ok(())
}

/// Stop and remove every service of the vault at `db`: the one its id names,
/// when the id is known, and every one that points at `db`.
pub fn uninstall(db: &Path, vault_id: Option<&str>, out: &mut dyn Write) -> Result<(), StoreError> {
    let home = home()?;
    let removed: Vec<String> = if cfg!(target_os = "macos") {
        let names = installed_for(&agents_dir(&home), PLIST, &plist_fragment(db), vault_id);
        for name in &names {
            remove_agent(&home, name)?;
        }
        names
    } else if cfg!(target_os = "linux") {
        let names = installed_for(&units_dir(&home), UNIT, &unit_fragment(db), vault_id);
        for name in &names {
            remove_unit(&home, name)?;
        }
        names.iter().map(|n| format!("{n}{UNIT}")).collect()
    } else {
        return Err(StoreError::invalid("no installer for this platform"));
    };
    if removed.is_empty() {
        writeln!(out, "no dreams service for {}", db.display()).map_err(io)?;
    }
    for name in removed {
        writeln!(out, "removed {name}").map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_and_unit_name_the_binary_and_database() {
        let plist = render_plist(
            "io.dreams.vault",
            Path::new("/opt/bin/dreams"),
            Path::new("/v/my vault.db"),
            Path::new("/l/vault.log"),
            "/opt/bin:/usr/bin",
        );
        assert!(plist.contains("<string>io.dreams.vault</string>"));
        assert!(plist.contains("<string>/opt/bin/dreams</string>\n    <string>--db</string>\n    <string>/v/my vault.db</string>\n    <string>daemon</string>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>/l/vault.log</string>"));

        let unit = render_unit(Path::new("/opt/bin/dreams"), Path::new("/v/vault.db"), "/opt/bin");
        assert!(unit.contains("ExecStart=/opt/bin/dreams --db /v/vault.db daemon\n"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("Environment=PATH=/opt/bin"));
    }

    #[test]
    fn each_vault_id_names_its_own_service() {
        assert_eq!(service_name("01a0"), "io.dreams.vault-01a0");
        assert_ne!(service_name("01a0"), service_name("01a1"));
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dreams-daemon-{}", crate::doc::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn services_are_found_by_the_database_they_run() {
        let dir = scratch();
        let exe = Path::new("/opt/bin/dreams");
        let db = Path::new("/v/work/vault.db");
        let other = Path::new("/v/home/vault.db");
        let log = Path::new("/l/x.log");
        let write = |file: &str, text: String| std::fs::write(dir.join(file), text).unwrap();
        // The new name, and the older names that install moves over.
        write("io.dreams.vault-01a0.plist", render_plist("io.dreams.vault-01a0", exe, db, log, ""));
        write("io.dreams.vault.plist", render_plist("io.dreams.vault", exe, db, log, ""));
        write("io.dreams.vault-01a1.plist", render_plist("io.dreams.vault-01a1", exe, other, log, ""));
        write("com.other.plist", "<plist><string>--db</string></plist>".into());
        write("io.dreams.vault-01a0.service", render_unit(exe, db, ""));
        write("dreams-vault.service", render_unit(exe, db, ""));
        write("dreams-vault-01a0.service", render_unit(exe, db, ""));
        write("dreams-vault-01a1.service", render_unit(exe, other, ""));

        let plists = |db: &Path| services_for(&dir, PLIST, &plist_fragment(db));
        assert_eq!(plists(db), ["io.dreams.vault", "io.dreams.vault-01a0"]);
        assert_eq!(plists(other), ["io.dreams.vault-01a1"]);
        let units = |db: &Path| services_for(&dir, UNIT, &unit_fragment(db));
        assert_eq!(units(db), ["dreams-vault", "dreams-vault-01a0", "io.dreams.vault-01a0"]);
        assert_eq!(units(other), ["dreams-vault-01a1"]);
        // A path that another path starts with is not the same database.
        assert_eq!(plists(Path::new("/v/work/vault")), Vec::<String>::new());
        assert_eq!(units(Path::new("/v/work/vault")), Vec::<String>::new());
        assert_eq!(services_for(&dir.join("missing"), PLIST, "x"), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_moved_vault_is_found_by_its_id() {
        let dir = scratch();
        let exe = Path::new("/opt/bin/dreams");
        let log = Path::new("/l/x.log");
        let old = render_plist("io.dreams.vault-01a0", exe, Path::new("/old/vault.db"), log, "");
        std::fs::write(dir.join("io.dreams.vault-01a0.plist"), old).unwrap();
        let fragment = plist_fragment(Path::new("/new/vault.db"));
        assert_eq!(installed_for(&dir, PLIST, &fragment, None), Vec::<String>::new());
        assert_eq!(installed_for(&dir, PLIST, &fragment, Some("01a0")), ["io.dreams.vault-01a0"]);
        assert_eq!(installed_for(&dir, PLIST, &fragment, Some("01a1")), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
