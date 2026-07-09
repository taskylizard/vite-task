use std::error::Error;

const USAGE: &str = "Usage: vtt small_dev_shm <command> [args...]";

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (program, command_args) = parse_command(args)?;
    run_platform(program, command_args)?;
    Ok(())
}

fn parse_command(args: &[String]) -> Result<(&str, &[String]), &'static str> {
    args.split_first().map(|(program, args)| (program.as_str(), args)).ok_or(USAGE)
}

#[cfg(target_os = "linux")]
fn write_procfs(path: &str, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(content.as_bytes())
}

#[cfg(target_os = "linux")]
fn run_platform(program: &str, command_args: &[String]) -> std::io::Result<()> {
    use std::{os::unix::process::ExitStatusExt as _, process::Command};

    use nix::{
        mount::{MsFlags, mount},
        sched::{CloneFlags, unshare},
        unistd::{Gid, Uid},
    };

    fn operation_error(operation: &str, error: impl std::fmt::Display) -> std::io::Error {
        std::io::Error::other(format!("{operation}: {error}"))
    }

    let uid = Uid::current().as_raw();
    let gid = Gid::current().as_raw();

    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .map_err(|error| operation_error("unshare user and mount namespaces", error))?;

    write_procfs("/proc/self/uid_map", &format!("0 {uid} 1\n"))
        .map_err(|error| operation_error("write /proc/self/uid_map", error))?;
    match write_procfs("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(operation_error("write /proc/self/setgroups", error)),
    }
    write_procfs("/proc/self/gid_map", &format!("0 {gid} 1\n"))
        .map_err(|error| operation_error("write /proc/self/gid_map", error))?;

    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>)
        .map_err(|error| operation_error("make / recursively private", error))?;

    mount(
        Some("tmpfs"),
        "/dev/shm",
        Some("tmpfs"),
        MsFlags::empty(),
        Some("nr_blocks=1,huge=never"),
    )
    .map_err(|error| operation_error("mount one-page tmpfs at /dev/shm", error))?;

    let status = Command::new(program).args(command_args).status()?;
    let code = status.code().unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal));
    std::process::exit(code);
}

#[cfg(not(target_os = "linux"))]
fn run_platform(_program: &str, _command_args: &[String]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "vtt small_dev_shm is only supported on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_a_command() {
        assert_eq!(parse_command(&[]).unwrap_err(), USAGE);
    }

    #[test]
    fn parses_command_and_arguments() {
        let args = ["vt".to_owned(), "run".to_owned(), "stress".to_owned()];
        let (program, command_args) = parse_command(&args).unwrap();

        assert_eq!(program, "vt");
        assert_eq!(command_args, ["run", "stress"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_writer_does_not_create_missing_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let missing = tempdir.path().join("missing");

        let error = write_procfs(missing.to_str().unwrap(), "content").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!missing.exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn reports_unsupported_platform() {
        let error = run_platform("vt", &[]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(error.to_string(), "vtt small_dev_shm is only supported on Linux");
    }
}
