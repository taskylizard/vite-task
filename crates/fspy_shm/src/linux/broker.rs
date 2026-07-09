use std::{
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, OwnedFd},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rustix::{
    cmsg_space,
    net::{
        AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags,
        SocketType, accept_with, bind, connect, listen, recvmsg, send, sendmsg, socket_with,
    },
};
use tokio::{io::unix::AsyncFd, time::timeout};
use uuid::Uuid;

use crate::ShmBroker;

const ID_PREFIX: &str = "fspy-memfd-v1";
const PROTOCOL_MAGIC: [u8; 4] = *b"VPSH";
const PROTOCOL_VERSION: u8 = 1;
const TOKEN_LEN: usize = 16;
const REQUEST_LEN: usize = PROTOCOL_MAGIC.len() + 1 + TOKEN_LEN;
const RESPONSE: [u8; 5] = [b'V', b'P', b'S', b'H', PROTOCOL_VERSION];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) fn new(memfd: OwnedFd) -> io::Result<(String, ShmBroker)> {
    let abstract_name = Uuid::new_v4().into_bytes();
    let token = Uuid::new_v4().into_bytes();
    let listener = new_socket(SocketFlags::CLOEXEC | SocketFlags::NONBLOCK)?;
    bind(&listener, &socket_address(&abstract_name)?).map_err(io::Error::from)?;
    listen(&listener, 16).map_err(io::Error::from)?;

    let id = encode_id(&abstract_name, &token);
    Ok((id, Box::pin(run_broker(listener, memfd, token))))
}

async fn run_broker(listener: OwnedFd, memfd: OwnedFd, token: [u8; TOKEN_LEN]) -> io::Result<()> {
    let listener = AsyncFd::new(listener)?;
    loop {
        let client = accept(&listener).await?;
        let _request_result =
            timeout(REQUEST_TIMEOUT, handle_request(client, &memfd, &token)).await;
    }
}

async fn accept(listener: &AsyncFd<OwnedFd>) -> io::Result<OwnedFd> {
    loop {
        let mut ready = listener.readable().await?;
        match ready.try_io(|inner| {
            accept_with(inner.get_ref(), SocketFlags::CLOEXEC | SocketFlags::NONBLOCK)
                .map_err(io::Error::from)
        }) {
            Ok(result) => return result,
            Err(_would_block) => {}
        }
    }
}

async fn handle_request(
    client: OwnedFd,
    memfd: &OwnedFd,
    token: &[u8; TOKEN_LEN],
) -> io::Result<()> {
    let client = AsyncFd::new(client)?;
    let request = receive_request(&client).await?;
    if !valid_request(&request, token) {
        return Ok(());
    }
    send_memfd(&client, memfd).await
}

async fn receive_request(client: &AsyncFd<OwnedFd>) -> io::Result<[u8; REQUEST_LEN]> {
    let mut request = [0_u8; REQUEST_LEN];
    loop {
        let mut ready = client.readable().await?;
        let result = ready.try_io(|inner| {
            let mut iov = [IoSliceMut::new(&mut request)];
            let mut ancillary = RecvAncillaryBuffer::default();
            recvmsg(
                inner.get_ref(),
                &mut iov,
                &mut ancillary,
                RecvFlags::DONTWAIT | RecvFlags::TRUNC,
            )
            .map_err(io::Error::from)
        });
        match result {
            Ok(Ok(message)) if message.bytes == REQUEST_LEN => return Ok(request),
            Ok(Ok(_message)) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed broker request"));
            }
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
    }
}

fn valid_request(request: &[u8; REQUEST_LEN], token: &[u8; TOKEN_LEN]) -> bool {
    request[..PROTOCOL_MAGIC.len()] == PROTOCOL_MAGIC
        && request[PROTOCOL_MAGIC.len()] == PROTOCOL_VERSION
        && request[PROTOCOL_MAGIC.len() + 1..] == token[..]
}

async fn send_memfd(client: &AsyncFd<OwnedFd>, memfd: &OwnedFd) -> io::Result<()> {
    loop {
        let mut ready = client.writable().await?;
        match ready.try_io(|inner| send_memfd_now(inner.get_ref(), memfd)) {
            Ok(result) => return result,
            Err(_would_block) => {}
        }
    }
}

fn send_memfd_now(client: &OwnedFd, memfd: &OwnedFd) -> io::Result<()> {
    let descriptor = [memfd.as_fd()];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !ancillary.push(SendAncillaryMessage::ScmRights(&descriptor)) {
        return Err(io::Error::other("failed to construct SCM_RIGHTS response"));
    }
    let sent = sendmsg(
        client,
        &[IoSlice::new(&RESPONSE)],
        &mut ancillary,
        SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
    )
    .map_err(io::Error::from)?;
    if sent != RESPONSE.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short broker response"));
    }
    Ok(())
}

pub(super) fn request_memfd(id: &str) -> io::Result<OwnedFd> {
    let (abstract_name, token) = decode_id(id)?;
    let socket = new_socket(SocketFlags::CLOEXEC)?;
    let address = socket_address(&abstract_name)?;
    connect(&socket, &address).map_err(io::Error::from)?;

    let mut request = [0_u8; REQUEST_LEN];
    request[..PROTOCOL_MAGIC.len()].copy_from_slice(&PROTOCOL_MAGIC);
    request[PROTOCOL_MAGIC.len()] = PROTOCOL_VERSION;
    request[PROTOCOL_MAGIC.len() + 1..].copy_from_slice(&token);
    let sent = send(&socket, &request, SendFlags::NOSIGNAL).map_err(io::Error::from)?;
    if sent != request.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short broker request"));
    }

    receive_memfd(&socket)
}

fn receive_memfd(socket: &OwnedFd) -> io::Result<OwnedFd> {
    let mut response = [0_u8; RESPONSE.len() + 1];
    let mut iov = [IoSliceMut::new(&mut response)];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let message = recvmsg(socket, &mut iov, &mut ancillary, RecvFlags::CMSG_CLOEXEC)
        .map_err(io::Error::from)?;
    if message.bytes != RESPONSE.len()
        || message.flags.intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC)
        || response[..RESPONSE.len()] != RESPONSE
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid broker response"));
    }

    let mut received = None;
    for message in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(descriptors) = message {
            for descriptor in descriptors {
                if received.replace(descriptor).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "broker returned multiple descriptors",
                    ));
                }
            }
        }
    }
    received
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "broker returned no descriptor"))
}

fn new_socket(flags: SocketFlags) -> io::Result<OwnedFd> {
    socket_with(AddressFamily::UNIX, SocketType::SEQPACKET, flags, None).map_err(io::Error::from)
}

fn socket_address(abstract_name: &[u8]) -> io::Result<SocketAddrUnix> {
    SocketAddrUnix::new_abstract_name(abstract_name).map_err(io::Error::from)
}

fn encode_id(abstract_name: &[u8], token: &[u8; TOKEN_LEN]) -> String {
    format!(
        "{ID_PREFIX}.{}.{}",
        URL_SAFE_NO_PAD.encode(abstract_name),
        URL_SAFE_NO_PAD.encode(token)
    )
}

fn decode_id(id: &str) -> io::Result<(Vec<u8>, [u8; TOKEN_LEN])> {
    let mut parts = id.split('.');
    if parts.next() != Some(ID_PREFIX) {
        return Err(invalid_id());
    }
    let abstract_name = parts.next().ok_or_else(invalid_id)?;
    let token = parts.next().ok_or_else(invalid_id)?;
    if parts.next().is_some() {
        return Err(invalid_id());
    }

    let abstract_name = URL_SAFE_NO_PAD.decode(abstract_name).map_err(|_| invalid_id())?;
    if abstract_name.is_empty() {
        return Err(invalid_id());
    }
    let token = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid_id())?;
    let token = token.try_into().map_err(|_| invalid_id())?;
    Ok((abstract_name, token))
}

fn invalid_id() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid shared-memory broker identifier")
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, IoSliceMut},
        process::Command,
    };

    use rustix::{
        fs::{SealFlags, fcntl_get_seals},
        io::{FdFlags, fcntl_getfd},
        net::{RecvAncillaryBuffer, RecvFlags, recvmsg},
    };
    use subprocess_test::command_for_fn;

    use super::*;

    #[test]
    fn abstract_identifiers_round_trip() {
        let abstract_name = b"fspy-\0\xff".to_vec();
        let token = [42; TOKEN_LEN];
        let id = encode_id(&abstract_name, &token);
        assert!(id.is_ascii());
        assert_eq!(decode_id(&id).unwrap(), (abstract_name.clone(), token));
        assert_eq!(
            socket_address(&abstract_name).unwrap().abstract_name(),
            Some(abstract_name.as_slice())
        );
    }

    #[test]
    fn broker_construction_is_independent_of_long_tmpdir() {
        let command = command_for_fn!((), |(): ()| {
            let created = crate::create(4096).unwrap();
            let (abstract_name, _) = decode_id(created.shm.id()).unwrap();
            assert_eq!(
                socket_address(&abstract_name).unwrap().abstract_name(),
                Some(abstract_name.as_slice())
            );
        });
        let status = std::thread::spawn(move || {
            Command::from(command)
                .env("TMPDIR", format!("/tmp/{}", "x".repeat(4096)))
                .status()
                .unwrap()
        })
        .join()
        .unwrap();
        assert!(status.success());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn received_descriptor_is_close_on_exec() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        let broker = tokio::spawn(created.broker);
        let descriptor = request_memfd_blocking(id).await.unwrap();
        assert!(fcntl_getfd(&descriptor).unwrap().contains(FdFlags::CLOEXEC));
        assert_eq!(
            fcntl_get_seals(&descriptor).unwrap(),
            SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL
        );
        abort_broker(broker).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn broker_serves_concurrent_opens() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        let broker = tokio::spawn(created.broker);
        let clients = (0..12)
            .map(|_| {
                let id = id.clone();
                tokio::task::spawn_blocking(move || crate::open(&id, 4096))
            })
            .collect::<Vec<_>>();

        for client in clients {
            assert_eq!(client.await.unwrap().unwrap().len(), 4096);
        }
        abort_broker(broker).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn brokers_are_isolated_by_abstract_name_and_token() {
        let first = crate::create(4096).unwrap();
        let second = crate::create(4096).unwrap();
        let first_id = first.shm.id().to_owned();
        let second_id = second.shm.id().to_owned();
        let first_broker = tokio::spawn(first.broker);
        let second_broker = tokio::spawn(second.broker);

        let (_, mut wrong_token) = decode_id(&first_id).unwrap();
        wrong_token[0] ^= u8::MAX;
        let (first_abstract_name, _) = decode_id(&first_id).unwrap();
        let wrong_id = encode_id(&first_abstract_name, &wrong_token);
        assert!(open_blocking(wrong_id, 4096).await.is_err());

        let first_opened = open_blocking(first_id, 4096).await.unwrap();
        let second_opened = open_blocking(second_id, 4096).await.unwrap();
        // SAFETY: Both mappings are live and the accesses are in bounds and synchronized.
        unsafe {
            first.shm.as_ptr().write(17);
            second.shm.as_ptr().write(29);
            assert_eq!(first_opened.as_ptr().read(), 17);
            assert_eq!(second_opened.as_ptr().read(), 29);
        }

        abort_broker(first_broker).await;
        abort_broker(second_broker).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn broker_abort_rejects_new_opens() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        let broker = tokio::spawn(created.broker);
        let opened = open_blocking(id.clone(), 4096).await.unwrap();
        abort_broker(broker).await;

        assert!(open_blocking(id, 4096).await.is_err());
        // SAFETY: The existing mapping remains live after broker shutdown.
        unsafe {
            opened.as_ptr().write(42);
            assert_eq!(opened.as_ptr().read(), 42);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn unstarted_broker_drop_releases_listener() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        drop(created.broker);

        assert!(open_blocking(id, 4096).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn size_mismatch_and_malformed_ids_are_rejected() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        let broker = tokio::spawn(created.broker);

        assert!(open_blocking(id.clone(), 8192).await.is_err());
        assert!(open_blocking("not-a-broker-id".to_owned(), 4096).await.is_err());
        assert!(open_blocking("fspy-memfd-v1...".to_owned(), 4096).await.is_err());
        assert!(open_blocking(id, 4096).await.is_ok());

        abort_broker(broker).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn malformed_and_unknown_requests_are_rejected_without_stopping_broker() {
        let created = crate::create(4096).unwrap();
        let id = created.shm.id().to_owned();
        let (abstract_name, token) = decode_id(&id).unwrap();
        let broker = tokio::spawn(created.broker);

        tokio::task::spawn_blocking(move || {
            assert_rejected(&abstract_name, b"bad");
            assert_rejected(&abstract_name, &[0_u8; REQUEST_LEN + 1]);
            let mut bad_version = [0_u8; REQUEST_LEN];
            bad_version[..PROTOCOL_MAGIC.len()].copy_from_slice(&PROTOCOL_MAGIC);
            bad_version[PROTOCOL_MAGIC.len()] = PROTOCOL_VERSION + 1;
            bad_version[PROTOCOL_MAGIC.len() + 1..].copy_from_slice(&token);
            assert_rejected(&abstract_name, &bad_version);
        })
        .await
        .unwrap();

        assert!(open_blocking(id, 4096).await.is_ok());
        abort_broker(broker).await;
    }

    async fn request_memfd_blocking(id: String) -> io::Result<OwnedFd> {
        tokio::task::spawn_blocking(move || request_memfd(&id)).await.unwrap()
    }

    async fn open_blocking(id: String, size: usize) -> io::Result<crate::Shm> {
        tokio::task::spawn_blocking(move || crate::open(&id, size)).await.unwrap()
    }

    async fn abort_broker(broker: tokio::task::JoinHandle<io::Result<()>>) {
        broker.abort();
        assert!(broker.await.unwrap_err().is_cancelled());
    }

    fn assert_rejected(abstract_name: &[u8], request: &[u8]) {
        let socket = new_socket(SocketFlags::CLOEXEC).unwrap();
        connect(&socket, &socket_address(abstract_name).unwrap()).unwrap();
        assert_eq!(send(&socket, request, SendFlags::NOSIGNAL).unwrap(), request.len());

        let mut response = [0_u8; RESPONSE.len()];
        let mut iov = [IoSliceMut::new(&mut response)];
        let mut ancillary = RecvAncillaryBuffer::default();
        let message = recvmsg(&socket, &mut iov, &mut ancillary, RecvFlags::CMSG_CLOEXEC).unwrap();
        assert_eq!(message.bytes, 0);
        assert!(ancillary.drain().next().is_none());
    }
}
