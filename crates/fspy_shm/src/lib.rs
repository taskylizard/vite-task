//! Platform shared-memory implementation for fspy channels.

#[cfg(target_os = "linux")]
use std::{future::Future, io, pin::Pin};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{CreatedShm, Shm, create, open};
#[cfg(target_os = "macos")]
pub use macos::{CreatedShm, Shm, create, open};
#[cfg(target_os = "windows")]
pub use windows::{CreatedShm, Shm, create, open};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("fspy_shm does not support this platform");

/// A Linux service future that makes a shared-memory mapping available to other processes.
#[cfg(target_os = "linux")]
pub type ShmBroker = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use std::{mem::align_of, process::Command};

    use subprocess_test::command_for_fn;

    use super::{Shm, create, open};

    const SIZE: usize = 64 * 1024;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn create_and_open_are_shared() {
        let owner = create(SIZE).unwrap().shm;
        verify_create_and_open(&owner);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_and_open_are_shared() {
        let created = create(SIZE).unwrap();
        let broker = tokio::spawn(created.broker);
        tokio::task::spawn_blocking(move || verify_create_and_open(&created.shm)).await.unwrap();
        abort_broker(broker).await;
    }

    fn verify_create_and_open(owner: &Shm) {
        assert_eq!(owner.len(), SIZE);
        assert_eq!(owner.as_ptr() as usize % align_of::<usize>(), 0);
        // SAFETY: No writes occur while this slice is borrowed.
        assert!(unsafe { owner.as_slice() }.iter().all(|byte| *byte == 0));

        let opened = open(owner.id(), SIZE).unwrap();
        assert_eq!(opened.id(), owner.id());
        assert_eq!(opened.len(), SIZE);

        write_byte(owner, 0, 17);
        assert_eq!(read_byte(&opened, 0), 17);
        write_byte(&opened, SIZE - 1, 29);
        assert_eq!(read_byte(owner, SIZE - 1), 29);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn mapping_is_visible_across_processes() {
        let owner = create(SIZE).unwrap().shm;
        verify_mapping_is_visible_across_processes(&owner);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn mapping_is_visible_across_processes() {
        let created = create(SIZE).unwrap();
        let broker = tokio::spawn(created.broker);
        tokio::task::spawn_blocking(move || {
            verify_mapping_is_visible_across_processes(&created.shm);
        })
        .await
        .unwrap();
        abort_broker(broker).await;
    }

    fn verify_mapping_is_visible_across_processes(owner: &Shm) {
        write_byte(owner, 0, 17);

        let command = command_for_fn!(owner.id().to_owned(), |id: String| {
            let opened = open(&id, SIZE).unwrap();
            assert_eq!(read_byte(&opened, 0), 17);
            write_byte(&opened, SIZE - 1, 29);
        });
        assert!(Command::from(command).status().unwrap().success());
        assert_eq!(read_byte(owner, SIZE - 1), 29);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn owner_drop_prevents_new_opens() {
        let owner = create(SIZE).unwrap().shm;
        let id = owner.id().to_owned();
        drop(owner);

        assert!(open(&id, SIZE).is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn broker_keeps_memfd_alive_after_owner_drop() {
        let created = create(SIZE).unwrap();
        let id = created.shm.id().to_owned();
        let broker = tokio::spawn(created.broker);
        drop(created.shm);

        assert!(
            tokio::task::spawn_blocking({
                let id = id.clone();
                move || open(&id, SIZE)
            })
            .await
            .unwrap()
            .is_ok()
        );
        abort_broker(broker).await;
        assert!(tokio::task::spawn_blocking(move || open(&id, SIZE)).await.unwrap().is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn opened_mapping_survives_owner_drop() {
        let owner = create(SIZE).unwrap().shm;
        verify_opened_mapping_survives_teardown(owner);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn opened_mapping_survives_owner_drop() {
        let created = create(SIZE).unwrap();
        let broker = tokio::spawn(created.broker);
        let owner = created.shm;
        let id = owner.id().to_owned();
        let open_id = id.clone();
        let opened =
            tokio::task::spawn_blocking(move || open(&open_id, SIZE)).await.unwrap().unwrap();
        write_byte(&owner, 0, 17);
        abort_broker(broker).await;
        drop(owner);

        assert!(tokio::task::spawn_blocking(move || open(&id, SIZE)).await.unwrap().is_err());
        assert_eq!(read_byte(&opened, 0), 17);
        write_byte(&opened, SIZE - 1, 29);
        assert_eq!(read_byte(&opened, SIZE - 1), 29);
    }

    #[cfg(not(target_os = "linux"))]
    fn verify_opened_mapping_survives_teardown(owner: Shm) {
        let id = owner.id().to_owned();
        let opened = open(&id, SIZE).unwrap();
        write_byte(&owner, 0, 17);
        drop(owner);

        // POSIX shm_unlink removes the name immediately. On Windows,
        // an existing mapped view keeps the named kernel object alive; fspy's
        // lock file, rather than this low-level mapping API, rejects late senders.
        #[cfg(not(target_os = "windows"))]
        assert!(open(&id, SIZE).is_err());
        assert_eq!(read_byte(&opened, 0), 17);
        write_byte(&opened, SIZE - 1, 29);
        assert_eq!(read_byte(&opened, SIZE - 1), 29);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn broker_runs_until_aborted() {
        let broker = tokio::spawn(create(SIZE).unwrap().broker);
        tokio::task::yield_now().await;
        assert!(!broker.is_finished());
        abort_broker(broker).await;
    }

    #[cfg(target_os = "linux")]
    async fn abort_broker(broker: tokio::task::JoinHandle<std::io::Result<()>>) {
        broker.abort();
        assert!(broker.await.unwrap_err().is_cancelled());
    }

    fn read_byte(shm: &Shm, index: usize) -> u8 {
        assert!(index < shm.len());
        // SAFETY: The index is in bounds and tests synchronize all accesses.
        unsafe { shm.as_ptr().add(index).read() }
    }

    fn write_byte(shm: &Shm, index: usize, value: u8) {
        assert!(index < shm.len());
        // SAFETY: The index is in bounds and tests synchronize all accesses.
        unsafe { shm.as_ptr().add(index).write(value) };
    }
}
