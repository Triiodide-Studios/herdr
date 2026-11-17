use std::io;
use std::path::{Path, PathBuf};

/// Legacy environment variable for overriding the client socket path.
///
/// Contractual override behavior for auto-detect uses `HERDR_SOCKET_PATH`.
/// This variable is kept as a fallback for callers that explicitly need a
/// client-only override when `HERDR_SOCKET_PATH` is not set.
pub const CLIENT_SOCKET_PATH_ENV_VAR: &str = "HERDR_CLIENT_SOCKET_PATH";

/// Socket permission mode (owner read/write only).
const SOCKET_PERMISSION_MODE: u32 = 0o600;

/// Returns the path for the client protocol socket.
///
/// Contract-aligned override behavior:
/// 1. If CLI `--session <name>` is active, use that session's client socket.
/// 2. If `HERDR_SOCKET_PATH` is set, derive the client socket path from it by
///    inserting `-client` before `.sock` (e.g. `herdr.sock` -> `herdr-client.sock`).
///    This keeps JSON API and client socket overrides consistent.
/// 3. Otherwise, honor `HERDR_CLIENT_SOCKET_PATH` (legacy/testing fallback).
/// 4. Otherwise, use the active session data directory.
pub fn client_socket_path() -> PathBuf {
    if crate::session::explicit_session_requested() {
        return crate::session::client_socket_path_for(crate::session::active_name().as_deref());
    }
    client_socket_path_from_overrides(
        std::env::var(crate::api::SOCKET_PATH_ENV_VAR)
            .ok()
            .as_deref(),
        std::env::var(CLIENT_SOCKET_PATH_ENV_VAR).ok().as_deref(),
    )
}

pub(crate) fn client_socket_path_from_overrides(
    api_socket_override: Option<&str>,
    client_socket_override: Option<&str>,
) -> PathBuf {
    if let Some(api_socket_override) = api_socket_override {
        return derive_client_socket_from_api_socket(Path::new(api_socket_override));
    }

    if let Some(client_socket_override) = client_socket_override {
        return PathBuf::from(client_socket_override);
    }

    crate::session::client_socket_path_for(crate::session::active_name().as_deref())
}

pub(crate) fn derive_client_socket_from_api_socket(api_socket_path: &Path) -> PathBuf {
    let stem = api_socket_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("herdr");
    let parent = api_socket_path.parent().unwrap_or_else(|| Path::new(""));

    parent.join(format!("{stem}-client.sock"))
}

/// Prepares a socket path for binding: creates parent directories,
/// removes stale socket files where no server is listening, and rejects live
/// sockets that are already in use.
pub(crate) fn prepare_socket_path(path: &Path) -> io::Result<()> {
    crate::ipc::prepare_socket_path(path, |path| {
        format!(
            "herdr server is already running (socket busy at {})",
            path.display()
        )
    })
}

/// Restricts socket file permissions to owner-only (0o600).
pub(crate) fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    crate::ipc::restrict_socket_permissions(path, SOCKET_PERMISSION_MODE)
}

/// Says which server already owns the API or client socket, or `None` when nothing does. A starting server asks before it
/// does anything of its own: its session restore spawns every pane, and its socket setup removes the files of a socket it
/// takes for stale. 10/30 13:03Z a second server started beside the live one, restored all its panes a second time, and
/// when it went away took the API socket file with it; the live server could not be reached until it was restarted.
///
/// A socket counts as stale only when connecting says nobody listens. One that answers, or fails any other way (busy,
/// timed out, refused to this user), belongs to a server. So does a marker written by a process that still runs: a server
/// whose listener has gone has not given up its sockets.
pub(crate) fn running_server_owner(api_path: &Path, client_path: &Path) -> Option<String> {
    for path in [client_path, api_path] {
        match crate::ipc::connect_local_stream(path) {
            Ok(_) => return Some(format!("{} answers", path.display())),
            Err(err) if crate::ipc::stale_socket_connect_error(err.kind()) => {}
            Err(err) => {
                return Some(format!(
                    "{} does not answer, and is not gone ({err})",
                    path.display()
                ))
            }
        }
    }
    [api_path, client_path].into_iter().find_map(|path| {
        crate::ipc::live_marker_owner(path)
            .map(|pid| format!("process {pid} owns {} and is running", path.display()))
    })
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    fn socket_in_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-owner-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("herdr.sock")
    }

    #[test]
    fn nothing_listening_is_no_server() {
        let api = socket_in_temp_dir("none-api");
        let client = socket_in_temp_dir("none-client");
        assert_eq!(running_server_owner(&api, &client), None);
    }

    #[test]
    fn a_socket_that_answers_is_a_running_server_on_either_path() {
        let api = socket_in_temp_dir("live-api");
        let client = socket_in_temp_dir("live-client");
        let listener = crate::ipc::bind_local_listener(&client).unwrap();
        let owner = running_server_owner(&api, &client).expect("the client socket answers");
        assert!(owner.contains("answers"), "{owner}");
        drop(listener);
        let _ = std::fs::remove_file(&client);

        let listener = crate::ipc::bind_local_listener(&api).unwrap();
        assert!(running_server_owner(&api, &client).is_some());
        drop(listener);
        let _ = std::fs::remove_file(&api);
        assert_eq!(
            running_server_owner(&api, &client),
            None,
            "the listener is gone and so is its file"
        );
    }

    #[test]
    fn a_socket_file_that_nobody_listens_on_is_stale() {
        let api = socket_in_temp_dir("stale-api");
        let client = socket_in_temp_dir("stale-client");
        let listener = crate::ipc::bind_local_listener(&api).unwrap();
        drop(listener);
        assert_eq!(running_server_owner(&api, &client), None);
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::process::{Child, Command, Stdio};

        fn sleeping_process() -> Child {
            Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        }

        fn unix_nanos_now() -> u128 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        }

        #[test]
        fn a_marker_of_a_running_process_is_a_server_though_its_listener_has_gone() {
            let api = socket_in_temp_dir("marker-api");
            let client = socket_in_temp_dir("marker-client");
            let mut server = sleeping_process();
            std::fs::write(&api, format!("{}:{}", server.id(), unix_nanos_now())).unwrap();

            let owner =
                running_server_owner(&api, &client).expect("a live process owns the marker");
            assert!(owner.contains(&server.id().to_string()), "{owner}");

            server.kill().unwrap();
            server.wait().unwrap();
            assert_eq!(
                running_server_owner(&api, &client),
                None,
                "the process is gone, so its marker is stale"
            );
        }

        #[test]
        fn a_marker_older_than_the_process_that_has_its_pid_is_stale() {
            let api = socket_in_temp_dir("reused-api");
            let client = socket_in_temp_dir("reused-client");
            let mut reused = sleeping_process();
            // written long before this process existed: the server that wrote it died and its pid was taken again
            std::fs::write(&api, format!("{}:{}", reused.id(), 1_000_000_000_u64)).unwrap();
            assert_eq!(running_server_owner(&api, &client), None);
            reused.kill().unwrap();
            reused.wait().unwrap();
        }

        #[test]
        fn a_marker_of_this_process_is_not_another_server() {
            let api = socket_in_temp_dir("self-api");
            let client = socket_in_temp_dir("self-client");
            std::fs::write(&api, format!("{}:{}", std::process::id(), unix_nanos_now())).unwrap();
            assert_eq!(running_server_owner(&api, &client), None);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    #[test]
    fn client_socket_path_derived_from_api_socket_override() {
        let path = client_socket_path_from_overrides(Some("/tmp/test-herdr.sock"), None);
        assert_eq!(path, PathBuf::from("/tmp/test-herdr-client.sock"));
    }

    #[test]
    fn client_socket_path_api_override_takes_precedence_over_legacy_client_override() {
        let path = client_socket_path_from_overrides(
            Some("/tmp/test-herdr.sock"),
            Some("/tmp/legacy-client.sock"),
        );
        assert_eq!(path, PathBuf::from("/tmp/test-herdr-client.sock"));
    }

    #[test]
    fn client_socket_path_respects_legacy_client_override_without_api_override() {
        let path = client_socket_path_from_overrides(None, Some("/tmp/test-herdr-client.sock"));
        assert_eq!(path, PathBuf::from("/tmp/test-herdr-client.sock"));
    }

    #[test]
    fn client_socket_path_defaults_to_config_dir() {
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let path = client_socket_path_from_overrides(None, None);
        assert_eq!(path, crate::config::config_dir().join("herdr-client.sock"));
    }

    #[test]
    fn derive_client_socket_from_api_socket_without_sock_extension() {
        let derived = derive_client_socket_from_api_socket(Path::new("/tmp/custom-api"));
        assert_eq!(derived, PathBuf::from("/tmp/custom-api-client.sock"));
    }

    #[test]
    fn prepare_socket_path_removes_stale_socket() {
        let dir = PathBuf::from(format!(
            "/tmp/hs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::create_dir_all(&dir);
        let socket_path = dir.join("stale.sock");

        {
            let _listener = UnixListener::bind(&socket_path).expect("bind stale socket");
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let result = prepare_socket_path(&socket_path);
        assert!(result.is_ok(), "should remove stale socket: {result:?}");
        assert!(!socket_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_path_rejects_live_socket() {
        let dir = PathBuf::from(format!(
            "/tmp/hl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::create_dir_all(&dir);
        let socket_path = dir.join("live.sock");

        let _listener = UnixListener::bind(&socket_path).expect("bind");

        let result = prepare_socket_path(&socket_path);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AddrInUse);

        let _ = fs::remove_dir_all(&dir);
    }
}
