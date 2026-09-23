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

fn stem(db: &Path) -> String {
    let raw = db.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "vault".into());
    let clean: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    if clean.is_empty() { "vault".into() } else { clean }
}

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
        "[Unit]\nDescription=subconscious daemon for {db}\n\n[Service]\nExecStart={exe} --db {db} daemon\nRestart=always\nRestartSec=5\nEnvironment=PATH={path}\n\n[Install]\nWantedBy=default.target\n",
        exe = exe.to_string_lossy(),
        db = db.to_string_lossy(),
        path = path_env,
    )
}

fn home() -> Result<PathBuf, StoreError> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| StoreError::invalid("HOME is not set"))
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

/// Register the daemon with the OS. `db` must be absolute.
pub fn install(db: &Path, exe: &Path, out: &mut dyn Write) -> Result<(), StoreError> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = home()?;
    let stem = stem(db);
    let io = |e: std::io::Error| StoreError::invalid(e.to_string());
    if cfg!(target_os = "macos") {
        let label = format!("io.subconscious.{stem}");
        let plist = home.join("Library/LaunchAgents").join(format!("{label}.plist"));
        let log = home.join("Library/Logs/subconscious").join(format!("{stem}.log"));
        std::fs::create_dir_all(plist.parent().unwrap()).map_err(io)?;
        std::fs::create_dir_all(log.parent().unwrap()).map_err(io)?;
        std::fs::write(&plist, render_plist(&label, exe, db, &log, &path_env)).map_err(io)?;
        let domain = format!("gui/{}", uid(&home)?);
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
        sh("launchctl", &["bootstrap".into(), domain, plist.to_string_lossy().into_owned()])?;
        writeln!(out, "installed {label}\n  {}\nlogs: {}", plist.display(), log.display()).map_err(io)?;
    } else if cfg!(target_os = "linux") {
        let name = format!("subconscious-{stem}.service");
        let unit = home.join(".config/systemd/user").join(&name);
        std::fs::create_dir_all(unit.parent().unwrap()).map_err(io)?;
        std::fs::write(&unit, render_unit(exe, db, &path_env)).map_err(io)?;
        sh("systemctl", &["--user".into(), "daemon-reload".into()])?;
        sh("systemctl", &["--user".into(), "enable".into(), "--now".into(), name.clone()])?;
        writeln!(
            out,
            "installed {name}\n  {}\nlogs: journalctl --user -u {name}\nTo keep it running while logged out: loginctl enable-linger",
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

pub fn uninstall(db: &Path, out: &mut dyn Write) -> Result<(), StoreError> {
    let home = home()?;
    let stem = stem(db);
    let io = |e: std::io::Error| StoreError::invalid(e.to_string());
    if cfg!(target_os = "macos") {
        let label = format!("io.subconscious.{stem}");
        let plist = home.join("Library/LaunchAgents").join(format!("{label}.plist"));
        let domain = format!("gui/{}", uid(&home)?);
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
        match std::fs::remove_file(&plist) {
            Ok(()) => writeln!(out, "removed {label}").map_err(io)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => writeln!(out, "{label} was not installed").map_err(io)?,
            Err(e) => return Err(io(e)),
        }
    } else if cfg!(target_os = "linux") {
        let name = format!("subconscious-{stem}.service");
        let unit = home.join(".config/systemd/user").join(&name);
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", &name]).output();
        match std::fs::remove_file(&unit) {
            Ok(()) => {
                let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
                writeln!(out, "removed {name}").map_err(io)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => writeln!(out, "{name} was not installed").map_err(io)?,
            Err(e) => return Err(io(e)),
        }
    } else {
        return Err(StoreError::invalid("no installer for this platform"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_and_unit_name_the_binary_and_database() {
        let plist = render_plist(
            "io.subconscious.vault",
            Path::new("/opt/bin/subconscious"),
            Path::new("/v/my vault.db"),
            Path::new("/l/vault.log"),
            "/opt/bin:/usr/bin",
        );
        assert!(plist.contains("<string>io.subconscious.vault</string>"));
        assert!(plist.contains("<string>/opt/bin/subconscious</string>\n    <string>--db</string>\n    <string>/v/my vault.db</string>\n    <string>daemon</string>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>/l/vault.log</string>"));

        let unit = render_unit(Path::new("/opt/bin/subconscious"), Path::new("/v/vault.db"), "/opt/bin");
        assert!(unit.contains("ExecStart=/opt/bin/subconscious --db /v/vault.db daemon\n"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("Environment=PATH=/opt/bin"));
    }

    #[test]
    fn stems_are_safe_labels() {
        assert_eq!(stem(Path::new("/a/vault.db")), "vault");
        assert_eq!(stem(Path::new("/a/my notes.sqlite")), "my-notes");
    }
}
