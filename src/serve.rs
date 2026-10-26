//! `bbs serve`: the server is its own executable, `bbs-server`, so `bbs`
//! carries none of it. This runs it with the arguments given.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const SERVER: &str = "bbs-server";

/// Whether `path` is a file this process may run.
fn runnable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// `bbs-server` beside this executable, as an install puts them, else on PATH.
fn server() -> Option<PathBuf> {
    let name = format!("{SERVER}{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(&name)))
        .filter(|path| runnable(path));
    beside.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&name))
            .find(|path| runnable(path))
    })
}

/// Run `bbs-server ARGS` in this process's place, so signals, the exit status
/// and the terminal are its own; without it, say how to get it.
pub fn exec(args: &[OsString]) -> ExitCode {
    let Some(server) = server() else {
        eprintln!(
            "bbs: `bbs serve` runs {SERVER}, which is not installed; install it beside `bbs` \
             with `cargo install --locked {SERVER}`"
        );
        return ExitCode::FAILURE;
    };
    let mut command = Command::new(&server);
    command.args(
        args.iter()
            .map(OsString::as_os_str)
            .collect::<Vec<&OsStr>>(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        eprintln!("bbs: running {}: {err}", server.display());
        ExitCode::FAILURE
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("bbs: running {}: {err}", server.display());
            ExitCode::FAILURE
        }
    }
}
