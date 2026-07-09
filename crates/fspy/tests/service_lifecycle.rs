#![cfg(all(target_os = "linux", not(target_env = "musl")))]

use std::{env::current_exe, io, process::Stdio};

use fspy::error::SpawnError;
use fspy_shared::ipc::channel::{CreatedChannel, channel};
use tokio_util::sync::CancellationToken;

#[test_log::test(tokio::test)]
async fn broker_runs_until_aborted() -> anyhow::Result<()> {
    let CreatedChannel { broker, .. } = channel(64 * 1024)?;
    let handle = tokio::spawn(broker);
    tokio::task::yield_now().await;
    assert!(!handle.is_finished());
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn injection_failure_aborts_broker() -> anyhow::Result<()> {
    let mut command = fspy::Command::new(current_exe()?);
    command.arg("--help").stdout(Stdio::null()).env("FSPY_PAYLOAD", "conflict");

    let result = command.spawn(CancellationToken::new()).await;
    assert!(matches!(result, Err(SpawnError::Injection(_))));
    Ok(())
}

#[test_log::test(tokio::test)]
async fn spawn_failure_aborts_broker() -> anyhow::Result<()> {
    let mut command = fspy::Command::new(current_exe()?);
    command.arg("--help").stdout(Stdio::null());
    // SAFETY: The closure only constructs an OS error and returns without touching process state.
    unsafe {
        command.pre_exec(|| Err(io::Error::from_raw_os_error(libc::EINVAL)));
    }

    let result = command.spawn(CancellationToken::new()).await;
    assert!(matches!(result, Err(SpawnError::OsSpawn(_))));
    Ok(())
}
