use std::path::{Path, PathBuf};

use tracing::warn;

use super::snapshot::{
    parse_history_snapshot, parse_snapshot, snapshot_file_version, SessionHistorySnapshot,
    SessionSnapshot, SNAPSHOT_VERSION,
};

pub(super) fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
}

// Follow symlinks manually so a write through a (possibly dangling) symlink
// lands on the target. `fs::canonicalize` requires the target to exist, which
// excludes the dangling-symlink case stow users hit on the very first save.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    save_json_to_path(path, snapshot)
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    let tmp_path = target.with_extension("json.tmp");
    // The data is durable before the rename publishes it: a power loss after an unflushed rename
    // leaves a zero-filled session file.
    let result = (|| {
        let mut file = std::fs::File::create(&tmp_path)?;
        std::io::Write::write_all(&mut file, json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        crate::platform::replace_file(&tmp_path, &target)?;
        crate::platform::sync_parent_directory(target.parent().unwrap_or_else(|| Path::new(".")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

pub(super) fn save_history_to_path(
    path: &Path,
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    match history {
        Some(history) => save_json_to_path(path, history),
        None => clear_path(path),
    }
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load() -> Option<SessionSnapshot> {
    load_from_path(&session_path())
}

enum SessionRead {
    Loaded(SessionSnapshot),
    Missing,
    NewerVersion,
    Unreadable,
}

/// A missing session file is a fresh or cleared session. An unreadable one (torn by a crash or
/// power loss) is replaced by the newest readable recovery copy beside it.
fn load_from_path(path: &Path) -> Option<SessionSnapshot> {
    match read_session(path) {
        SessionRead::Loaded(snapshot) => Some(snapshot),
        SessionRead::Missing | SessionRead::NewerVersion => None,
        SessionRead::Unreadable => recover_from_copies(path),
    }
}

fn recover_from_copies(path: &Path) -> Option<SessionSnapshot> {
    let mut copies: Vec<(u128, PathBuf)> = ["session-snapshots", "session-backups"]
        .into_iter()
        .filter_map(|directory| super::writer::recovery_files(&path.with_file_name(directory)).ok())
        .flatten()
        .collect();
    copies.sort();
    for (_, copy) in copies.iter().rev() {
        let Ok(content) = std::fs::read_to_string(copy) else {
            continue;
        };
        let Ok(snapshot) = parse_snapshot(&content) else {
            continue;
        };
        if snapshot.workspaces.is_empty() {
            continue;
        }
        warn!(
            event = "persist.restore", subsystem = "persist", outcome = "recovered",
            path = %path.display(), recovery_path = %copy.display(),
            "session file is unreadable, restored the newest readable recovery copy"
        );
        return Some(snapshot);
    }
    warn!(
        event = "persist.restore", subsystem = "persist", outcome = "unrecoverable",
        path = %path.display(), copies = copies.len(),
        "session file is unreadable and no recovery copy is readable"
    );
    None
}

fn read_session(path: &Path) -> SessionRead {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return SessionRead::Missing;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), err = %err, "failed to read session file"
            );
            return SessionRead::Unreadable;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => SessionRead::Loaded(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        event = "persist.restore", subsystem = "persist", outcome = "unsupported_version",
                        path = %path.display(), file_version = version, supported = SNAPSHOT_VERSION,
                        "session file is from a newer herdr version, ignoring"
                    );
                    return SessionRead::NewerVersion;
                }
            }
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), err = %err, "failed to parse session file"
            );
            SessionRead::Unreadable
        }
    }
}

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session history file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    fn temp_session_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-session-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        }
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret"))).unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret"))).unwrap();

        save_history_to_path(&history_path, None).unwrap();

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    fn fixture_snapshot() -> SessionSnapshot {
        parse_snapshot(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .unwrap()
    }

    fn write_recovery_copy(session: &Path, directory: &str, timestamp: u128, content: &[u8]) {
        let directory = session.with_file_name(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("session-{timestamp:039}-1-0.json")),
            content,
        )
        .unwrap();
    }

    #[test]
    fn zero_filled_session_restores_newest_readable_copy() {
        let path = temp_session_path("zero-filled");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let mut older = fixture_snapshot();
        older.sidebar_width = Some(1);
        let mut newest = fixture_snapshot();
        newest.sidebar_width = Some(2);
        write_recovery_copy(&path, "session-snapshots", 10, &serde_json::to_vec(&older).unwrap());
        write_recovery_copy(&path, "session-snapshots", 20, &serde_json::to_vec(&newest).unwrap());
        // A torn backup newer than both is skipped.
        write_recovery_copy(&path, "session-backups", 30, &[0u8; 512]);

        let restored = load_from_path(&path).expect("recovered session");

        assert_eq!(restored.sidebar_width, Some(2));
        assert!(!restored.workspaces.is_empty());
    }

    #[test]
    fn unreadable_session_skips_empty_recovery_copies() {
        let path = temp_session_path("empty-copy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"version\":").unwrap();
        let mut full = fixture_snapshot();
        full.sidebar_width = Some(3);
        write_recovery_copy(&path, "session-snapshots", 10, &serde_json::to_vec(&full).unwrap());
        write_recovery_copy(&path, "session-backups", 20, &serde_json::to_vec(&empty_snapshot()).unwrap());

        assert_eq!(load_from_path(&path).expect("recovered session").sidebar_width, Some(3));
    }

    #[test]
    fn missing_session_is_not_recovered() {
        let path = temp_session_path("missing-with-copies");
        write_recovery_copy(&path, "session-snapshots", 10, &serde_json::to_vec(&fixture_snapshot()).unwrap());

        assert!(load_from_path(&path).is_none());
    }

    #[test]
    fn unreadable_session_without_copies_starts_fresh() {
        let path = temp_session_path("no-copies");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 64]).unwrap();

        assert!(load_from_path(&path).is_none());
    }

    #[test]
    fn save_to_path_replaces_existing_session() {
        let path = temp_session_path("replace");
        save_to_path(&path, &empty_snapshot()).unwrap();
        let mut snapshot = fixture_snapshot();
        snapshot.sidebar_width = Some(5);

        save_to_path(&path, &snapshot).unwrap();

        assert_eq!(load_from_path(&path).unwrap().sidebar_width, Some(5));
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(parsed.selected, 7);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }
}
