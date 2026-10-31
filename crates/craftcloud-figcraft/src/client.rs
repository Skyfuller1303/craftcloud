use std::{
    fmt,
    io::{Cursor, Read},
    net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    ops::{Deref, DerefMut},
    sync::{
        Condvar, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use figcraft_collab::{
    Client as SessionClient, Replica,
    message::{
        self, Access as WireAccess, Admin, AdminReply, CatchUp, ClientMsg, Manage, PROTOCOL,
        ServerMsg,
    },
};
use figcraft_doc::Document;
use tungstenite::{
    Message as WsMessage, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::Error;

const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_MESSAGES: usize = 64;
const MAX_ADDRESSES: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const ENDPOINT_PATH_MAX: usize = 256;
static DNS_RESOLVER_BUSY: Mutex<bool> = Mutex::new(false);
static DNS_RESOLVER_READY: Condvar = Condvar::new();

struct ResolverPermit;

impl ResolverPermit {
    fn acquire(deadline: Instant) -> Result<Self, Error> {
        let mut busy = match DNS_RESOLVER_BUSY.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if !*busy {
                *busy = true;
                return Ok(Self);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::RemoteUnavailable);
            }
            let (next, timeout) = match DNS_RESOLVER_READY.wait_timeout(busy, remaining) {
                Ok(result) => result,
                Err(poisoned) => poisoned.into_inner(),
            };
            busy = next;
            if timeout.timed_out() && *busy {
                return Err(Error::RemoteUnavailable);
            }
        }
    }
}

impl Drop for ResolverPermit {
    fn drop(&mut self) {
        let mut busy = match DNS_RESOLVER_BUSY.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *busy = false;
        DNS_RESOLVER_READY.notify_one();
    }
}

pub(super) fn resolve_addresses_with<F>(
    host: &str,
    port: u16,
    deadline: Instant,
    resolver: F,
) -> Result<Vec<SocketAddr>, Error>
where
    F: FnOnce(&str, u16) -> Result<Vec<SocketAddr>, ()> + Send + 'static,
{
    if Instant::now() >= deadline {
        return Err(Error::RemoteUnavailable);
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let permit = ResolverPermit::acquire(deadline)?;
    let host = host.to_owned();
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("figcraft-dns-resolver".to_owned())
        .spawn(move || {
            let result = resolver(&host, port).map(|addresses| {
                addresses
                    .into_iter()
                    .take(MAX_ADDRESSES)
                    .collect::<Vec<_>>()
            });
            let _ = send.send(result);
            drop(permit);
        })
        .map_err(|_| Error::RemoteUnavailable)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::RemoteUnavailable);
    }
    let addresses = receive
        .recv_timeout(remaining)
        .map_err(|_| Error::RemoteUnavailable)?
        .map_err(|_| Error::RemoteUnavailable)?;
    if Instant::now() >= deadline || addresses.is_empty() {
        Err(Error::RemoteUnavailable)
    } else {
        Ok(addresses)
    }
}

pub(super) struct Socket {
    websocket: WebSocket<MaybeTlsStream<TcpStream>>,
    deadline: SocketDeadline,
}

impl Socket {
    fn check_deadline(&self) -> Result<(), Error> {
        self.deadline.check()
    }
}

impl Deref for Socket {
    type Target = WebSocket<MaybeTlsStream<TcpStream>>;

    fn deref(&self) -> &Self::Target {
        &self.websocket
    }
}

impl DerefMut for Socket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.websocket
    }
}

struct SocketDeadline {
    at: Instant,
    cancel: Option<SyncSender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl SocketDeadline {
    fn start(stream: &TcpStream, at: Instant) -> Result<Self, Error> {
        let stream = stream.try_clone().map_err(|_| Error::RemoteUnavailable)?;
        let (cancel, wait) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("figcraft-socket-deadline".to_owned())
            .spawn(move || {
                if matches!(
                    wait.recv_timeout(at.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            })
            .map_err(|_| Error::RemoteUnavailable)?;
        Ok(Self {
            at,
            cancel: Some(cancel),
            worker: Some(worker),
        })
    }

    fn check(&self) -> Result<(), Error> {
        if Instant::now() >= self.at {
            Err(Error::RemoteUnavailable)
        } else {
            Ok(())
        }
    }
}

impl Drop for SocketDeadline {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn operation_deadline() -> Instant {
    Instant::now() + OPERATION_TIMEOUT
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Edit,
    View,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::View => "view",
        }
    }

    fn wire(self) -> WireAccess {
        match self {
            Self::Edit => WireAccess::Edit,
            Self::View => WireAccess::View,
        }
    }
}

pub struct Link {
    pub id: String,
    pub token: String,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Export {
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Export {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Export")
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

pub trait Backend: Send + Sync {
    fn publish(&self, original: &[u8], name: &str) -> Result<String, Error>;
    fn create_link(&self, file: &str, access: Access) -> Result<Link, Error>;
    fn revoke_link(&self, file: &str, link: &str) -> Result<(), Error>;
    fn export(&self, file: &str, original: &[u8]) -> Result<Export, Error>;
    fn remove(&self, file: &str) -> Result<(), Error>;
}

pub struct Client {
    endpoint: Url,
    key: Zeroizing<String>,
}

impl Client {
    pub fn new(internal_url: &str, key: String) -> Result<Self, Error> {
        let endpoint = Url::parse(internal_url).map_err(|_| Error::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "ws" | "wss")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path().len() > ENDPOINT_PATH_MAX
        {
            return Err(Error::InvalidEndpoint);
        }
        if key.is_empty()
            || key.len() > 256
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::InvalidKey);
        }
        Ok(Self {
            endpoint,
            key: Zeroizing::new(key),
        })
    }

    pub(super) fn request_admin(
        &self,
        request: Admin,
        deadline: Instant,
    ) -> Result<AdminReply, Error> {
        let mut socket = connect_socket(&self.endpoint, deadline)?;
        let manage = Manage {
            protocol: PROTOCOL,
            key: self.key.to_string(),
            client_version: message::client_version(),
        };
        send_client(&mut socket, ClientMsg::Manage(manage))?;
        match read_admin_reply(&mut socket)? {
            AdminReply::Ready => {}
            _ => return Err(Error::RemoteUnavailable),
        }
        send_client(&mut socket, ClientMsg::Admin(request))?;
        read_admin_reply(&mut socket)
    }

    pub(super) fn create_link_inner(
        &self,
        file: &str,
        access: Access,
        label: &str,
        deadline: Instant,
    ) -> Result<Link, Error> {
        validate_file_id(file)?;
        let reply = self.request_admin(
            Admin::CreateLink {
                file: file.to_string(),
                access: access.wire(),
                label: label.to_string(),
            },
            deadline,
        )?;
        match reply {
            AdminReply::Link(link) => {
                let token = Zeroizing::new(link.token);
                if !valid_id(&link.link.id)
                    || link.link.file != file
                    || token.is_empty()
                    || token.len() > 128
                    || !token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(Error::Protocol);
                }
                Ok(Link {
                    id: link.link.id,
                    token: token.to_string(),
                })
            }
            _ => Err(Error::RemoteUnavailable),
        }
    }

    fn revoke_internal(&self, file: &str, link: &str, deadline: Instant) -> Result<(), Error> {
        validate_file_id(file)?;
        validate_link_id(link)?;
        match self.request_admin(
            Admin::Revoke {
                file: file.to_string(),
                link: link.to_string(),
            },
            deadline,
        )? {
            AdminReply::Done => Ok(()),
            AdminReply::Failed { reason }
                if reason == "this file isn't on the server (it may have been removed)" =>
            {
                Ok(())
            }
            AdminReply::Failed { reason } if reason == "that file has no such link" => Ok(()),
            _ => Err(Error::RemoteUnavailable),
        }
    }

    fn remove_internal(&self, file: &str, deadline: Instant) -> Result<(), Error> {
        validate_file_id(file)?;
        match self.request_admin(
            Admin::Remove {
                file: file.to_string(),
            },
            deadline,
        )? {
            AdminReply::Done => Ok(()),
            AdminReply::Failed { reason }
                if reason == "this file isn't on the server (it may have been removed)" =>
            {
                Ok(())
            }
            _ => Err(Error::RemoteUnavailable),
        }
    }

    pub(super) fn export_link_label(&self, file: &str) -> String {
        use sha2::{Digest, Sha256};

        let mut digest = Sha256::new();
        digest.update(self.key.as_bytes());
        digest.update([0]);
        digest.update(file.as_bytes());
        let suffix: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("CraftCloud export recovery {suffix}")
    }

    fn cleanup_export_links(
        &self,
        file: &str,
        label: &str,
        deadline: Instant,
    ) -> Result<(), Error> {
        let links = match self.request_admin(
            Admin::Links {
                file: file.to_owned(),
            },
            deadline,
        )? {
            AdminReply::Links(links) => links,
            _ => return Err(Error::RemoteUnavailable),
        };
        for link in links
            .into_iter()
            .filter(|link| link.file == file && link.label == label)
        {
            validate_link_id(&link.id)?;
            self.revoke_internal(file, &link.id, deadline)?;
        }
        Ok(())
    }

    pub(super) fn join_snapshot(
        &self,
        file: &str,
        token: &str,
        deadline: Instant,
    ) -> Result<(Socket, SessionClient), Error> {
        validate_file_id(file)?;
        if token.is_empty()
            || token.len() > 128
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::InvalidToken);
        }
        let mut socket = connect_socket(&self.endpoint, deadline)?;
        send_client(
            &mut socket,
            SessionClient::hello(file, token, "CraftCloud", "#607D8B"),
        )?;
        for _ in 0..MAX_MESSAGES {
            match read_server_msg(&mut socket)? {
                msg @ ServerMsg::Welcome {
                    catch_up: CatchUp::Snapshot(_),
                    ..
                } => {
                    let client = SessionClient::join(msg).map_err(|_| Error::Protocol)?;
                    return Ok((socket, client));
                }
                ServerMsg::Welcome { .. } => return Err(Error::Protocol),
                ServerMsg::Refused { .. } => return Err(Error::RemoteUnavailable),
                _ => {}
            }
        }
        Err(Error::Protocol)
    }

    fn inspect_original(original: &[u8]) -> Result<OriginalArchive, Error> {
        if original.is_empty() || original.len() > MAX_ARCHIVE_BYTES {
            return Err(Error::LimitExceeded);
        }
        let (loaded, thumbnail) = archive_metadata(original)?;
        if !loaded.issues.is_empty() || loaded.images_past_budget != 0 {
            return Err(Error::UnsupportedArchive);
        }
        Ok(OriginalArchive {
            doc: loaded.doc,
            versions: loaded.versions,
            branches: loaded.branches,
            thumbnail,
        })
    }

    fn export_from_link(
        &self,
        file: &str,
        link: &Link,
        original: &[u8],
        deadline: Instant,
    ) -> Result<Export, Error> {
        let source = Self::inspect_original(original)?;
        let (_, session) = self.join_snapshot(file, &link.token, deadline)?;
        let mut current = session.document().clone();
        current.published = source.doc.published.clone();
        let bytes = figcraft_format::save_bytes_with_history(
            &current,
            source.thumbnail.as_deref(),
            &source.versions,
            &source.branches,
        )
        .map_err(|_| Error::InvalidDocument)?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(Error::LimitExceeded);
        }
        verify_export(&current, &source, &bytes)?;
        Ok(Export { bytes })
    }
}

impl Backend for Client {
    fn publish(&self, original: &[u8], _name: &str) -> Result<String, Error> {
        let deadline = operation_deadline();
        let source = Self::inspect_original(original)?;
        let snapshot = Replica::new(source.doc.clone())
            .snapshot()
            .map_err(|_| Error::InvalidDocument)?;
        let name = format!("CraftCloud-{}", uuid::Uuid::new_v4().simple());
        let reply = self.request_admin(Admin::Publish { name, snapshot }, deadline)?;
        let AdminReply::Published { file, edit, view } = reply else {
            return Err(Error::RemoteUnavailable);
        };
        let mut edit_token = Zeroizing::new(edit.token);
        let mut view_token = Zeroizing::new(view.token);
        if !valid_id(&file.id) {
            edit_token.zeroize();
            view_token.zeroize();
            return Err(Error::Protocol);
        }
        let edit_id = edit.link.id.clone();
        let view_id = view.link.id.clone();
        let edit_result = self.revoke_internal(&file.id, &edit_id, deadline);
        let view_result = self.revoke_internal(&file.id, &view_id, deadline);
        edit_token.zeroize();
        view_token.zeroize();
        if edit_result.is_err() || view_result.is_err() {
            let _ = self.remove_internal(&file.id, deadline);
            return Err(Error::RemoteUnavailable);
        }
        Ok(file.id)
    }

    fn create_link(&self, file: &str, access: Access) -> Result<Link, Error> {
        let deadline = operation_deadline();
        let label = match access {
            Access::Edit => "CraftCloud edit",
            Access::View => "CraftCloud view",
        };
        self.create_link_inner(file, access, label, deadline)
    }

    fn revoke_link(&self, file: &str, link: &str) -> Result<(), Error> {
        self.revoke_internal(file, link, operation_deadline())
    }

    fn export(&self, file: &str, original: &[u8]) -> Result<Export, Error> {
        let deadline = operation_deadline();
        validate_file_id(file)?;
        let label = self.export_link_label(file);
        self.cleanup_export_links(file, &label, deadline)?;
        let temporary = self.create_link_inner(file, Access::View, &label, deadline)?;
        let exported = self.export_from_link(file, &temporary, original, deadline);
        let revoked = self.revoke_internal(file, &temporary.id, deadline);
        match (exported, revoked) {
            (Ok(result), Ok(())) => Ok(result),
            _ => Err(Error::RemoteUnavailable),
        }
    }

    fn remove(&self, file: &str) -> Result<(), Error> {
        self.remove_internal(file, operation_deadline())
    }
}

pub(super) struct OriginalArchive {
    pub(super) doc: Document,
    pub(super) versions: Vec<figcraft_format::Version>,
    pub(super) branches: Vec<figcraft_format::Branch>,
    pub(super) thumbnail: Option<Vec<u8>>,
}

fn valid_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn validate_file_id(value: &str) -> Result<(), Error> {
    if valid_id(value) {
        Ok(())
    } else {
        Err(Error::InvalidIdentifier)
    }
}

fn validate_link_id(value: &str) -> Result<(), Error> {
    if valid_id(value) {
        Ok(())
    } else {
        Err(Error::InvalidIdentifier)
    }
}

pub(super) fn connect_socket(endpoint: &Url, deadline: Instant) -> Result<Socket, Error> {
    if Instant::now() >= deadline {
        return Err(Error::RemoteUnavailable);
    }
    let host = endpoint.host_str().ok_or(Error::InvalidEndpoint)?;
    let port = endpoint
        .port_or_known_default()
        .ok_or(Error::InvalidEndpoint)?;
    let addresses = resolve_addresses_with(host, port, deadline, |host, port| {
        (host, port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect())
            .map_err(|_| ())
    })?;
    let mut stream = None;
    for address in addresses {
        let remaining = CONNECT_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        if remaining.is_zero() {
            return Err(Error::RemoteUnavailable);
        }
        if let Ok(connected) = TcpStream::connect_timeout(&address, remaining) {
            stream = Some(connected);
            break;
        }
    }
    let stream = stream.ok_or(Error::RemoteUnavailable)?;
    let socket_deadline = SocketDeadline::start(&stream, deadline)?;
    stream
        .set_nodelay(true)
        .map_err(|_| Error::RemoteUnavailable)?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| Error::RemoteUnavailable)?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| Error::RemoteUnavailable)?;
    let request = endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| Error::InvalidEndpoint)?;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_ARCHIVE_BYTES);
    config.max_frame_size = Some(MAX_ARCHIVE_BYTES);
    let (websocket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|_| Error::RemoteUnavailable)?;
    socket_deadline.check()?;
    Ok(Socket {
        websocket,
        deadline: socket_deadline,
    })
}

pub(super) fn send_client(socket: &mut Socket, mut client: ClientMsg) -> Result<(), Error> {
    socket.check_deadline()?;
    let encoded = message::encode(&client);
    match &mut client {
        ClientMsg::Manage(manage) => manage.key.zeroize(),
        ClientMsg::Hello(hello) => hello.token.zeroize(),
        _ => {}
    }
    let mut bytes = encoded.map_err(|_| Error::Protocol)?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        bytes.zeroize();
        return Err(Error::LimitExceeded);
    }
    let frame = std::mem::take(&mut bytes);
    let result = socket.send(WsMessage::Binary(frame.into()));
    bytes.zeroize();
    result.map_err(|_| Error::RemoteUnavailable)?;
    socket.check_deadline()
}

pub(super) fn read_server_msg(socket: &mut Socket) -> Result<ServerMsg, Error> {
    socket.check_deadline()?;
    let message = socket.read().map_err(|_| Error::RemoteUnavailable)?;
    socket.check_deadline()?;
    match message {
        WsMessage::Binary(bytes) => {
            message::decode(&bytes, MAX_ARCHIVE_BYTES).map_err(|_| Error::Protocol)
        }
        WsMessage::Ping(bytes) => {
            socket
                .send(WsMessage::Pong(bytes))
                .map_err(|_| Error::RemoteUnavailable)?;
            Ok(ServerMsg::Pong)
        }
        WsMessage::Close(_) => Err(Error::RemoteUnavailable),
        WsMessage::Text(_) => Err(Error::Protocol),
        WsMessage::Pong(_) | WsMessage::Frame(_) => Ok(ServerMsg::Pong),
    }
}

fn read_admin_reply(socket: &mut Socket) -> Result<AdminReply, Error> {
    for _ in 0..MAX_MESSAGES {
        match read_server_msg(socket)? {
            ServerMsg::Admin(reply) => return Ok(*reply),
            ServerMsg::Refused { .. } => return Err(Error::RemoteUnavailable),
            _ => {}
        }
    }
    Err(Error::Protocol)
}

pub(super) fn archive_metadata(
    bytes: &[u8],
) -> Result<(figcraft_format::Loaded, Option<Vec<u8>>), Error> {
    if bytes.first().is_some_and(|b| *b == b'{') {
        let loaded =
            figcraft_format::load_bytes_with_versions(bytes).map_err(|_| Error::InvalidDocument)?;
        return Ok((loaded, None));
    }
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| Error::UnsupportedArchive)?;
    if archive.len() > 100_000 {
        return Err(Error::LimitExceeded);
    }
    let mut total = 0u64;
    let mut names = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| Error::UnsupportedArchive)?;
        if !names.insert(entry.name().to_string()) || !known_archive_entry(entry.name()) {
            return Err(Error::UnsupportedArchive);
        }
        total = total
            .checked_add(entry.size())
            .ok_or(Error::LimitExceeded)?;
        if total > MAX_ARCHIVE_BYTES as u64 {
            return Err(Error::LimitExceeded);
        }
    }
    let thumbnail = match archive.by_name("thumbnail.png") {
        Ok(entry) => {
            if entry.size() > MAX_ARCHIVE_BYTES as u64 {
                return Err(Error::LimitExceeded);
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .take((MAX_ARCHIVE_BYTES as u64) + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::UnsupportedArchive)?;
            if bytes.len() > MAX_ARCHIVE_BYTES {
                return Err(Error::LimitExceeded);
            }
            Some(bytes)
        }
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(_) => return Err(Error::UnsupportedArchive),
    };
    let loaded =
        figcraft_format::load_bytes_with_versions(bytes).map_err(|_| Error::InvalidDocument)?;
    if !loaded.issues.is_empty() || loaded.images_past_budget != 0 {
        return Err(Error::UnsupportedArchive);
    }
    Ok((loaded, thumbnail))
}

pub(super) fn known_archive_entry(name: &str) -> bool {
    if matches!(
        name,
        "document.json"
            | "thumbnail.png"
            | "versions/index.json"
            | "branches/index.json"
            | "library/manifest.json"
            | "library/document.json"
    ) {
        return true;
    }
    let image = name
        .strip_prefix("images/")
        .or_else(|| name.strip_prefix("library/images/"));
    if let Some(image) = image {
        return figcraft_doc::ImageData::is_valid_key(image);
    }
    if let Some(version) = name
        .strip_prefix("versions/")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return figcraft_format::is_valid_version_id(version);
    }
    let Some(rest) = name.strip_prefix("branches/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let Some(id) = parts.next() else { return false };
    if !figcraft_format::is_valid_version_id(id) {
        return false;
    }
    match (parts.next(), parts.next(), parts.next()) {
        (Some("base.json" | "head.json"), None, None) => true,
        (Some("versions"), Some("index.json"), None) => true,
        (Some("versions"), Some(file), None) => file
            .strip_suffix(".json")
            .is_some_and(figcraft_format::is_valid_version_id),
        _ => false,
    }
}

pub(super) fn verify_export(
    current: &Document,
    source: &OriginalArchive,
    bytes: &[u8],
) -> Result<(), Error> {
    let loaded =
        figcraft_format::load_bytes_with_versions(bytes).map_err(|_| Error::InvalidDocument)?;
    if !loaded.issues.is_empty()
        || loaded.images_past_budget != 0
        || loaded.versions.len() != source.versions.len()
        || loaded.branches.len() != source.branches.len()
        || !same_document(&loaded.doc, current)?
        || archive_thumbnail(bytes)? != source.thumbnail
    {
        return Err(Error::UnsupportedArchive);
    }
    for (before, after) in source.versions.iter().zip(loaded.versions.iter()) {
        if before.id != after.id
            || before.name != after.name
            || before.description != after.description
            || before.time != after.time
            || before.auto != after.auto
        {
            return Err(Error::UnsupportedArchive);
        }
        let before_doc = before.document().map_err(|_| Error::UnsupportedArchive)?;
        let after_doc = after.document().map_err(|_| Error::UnsupportedArchive)?;
        if !same_document(before_doc.as_ref(), after_doc.as_ref())? {
            return Err(Error::UnsupportedArchive);
        }
    }
    for (before, after) in source.branches.iter().zip(loaded.branches.iter()) {
        if before.id != after.id
            || before.name != after.name
            || before.time != after.time
            || before.client != after.client
            || before.state != after.state
            || !same_document(before.base.as_ref(), after.base.as_ref())?
            || !same_document(before.head.as_ref(), after.head.as_ref())?
            || before.versions.len() != after.versions.len()
        {
            return Err(Error::UnsupportedArchive);
        }
        for (before_version, after_version) in before.versions.iter().zip(after.versions.iter()) {
            if before_version.id != after_version.id
                || before_version.name != after_version.name
                || before_version.description != after_version.description
                || before_version.time != after_version.time
                || before_version.auto != after_version.auto
            {
                return Err(Error::UnsupportedArchive);
            }
            let before_doc = before_version
                .document()
                .map_err(|_| Error::UnsupportedArchive)?;
            let after_doc = after_version
                .document()
                .map_err(|_| Error::UnsupportedArchive)?;
            if !same_document(before_doc.as_ref(), after_doc.as_ref())? {
                return Err(Error::UnsupportedArchive);
            }
        }
    }
    Ok(())
}

fn same_document(left: &Document, right: &Document) -> Result<bool, Error> {
    Ok(
        serde_json::to_vec(left).map_err(|_| Error::UnsupportedArchive)?
            == serde_json::to_vec(right).map_err(|_| Error::UnsupportedArchive)?
            && left.images == right.images
            && left.published == right.published,
    )
}

fn archive_thumbnail(bytes: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    if bytes.first().is_some_and(|b| *b == b'{') {
        return Ok(None);
    }
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| Error::UnsupportedArchive)?;
    match archive.by_name("thumbnail.png") {
        Ok(entry) => {
            let mut bytes = Vec::new();
            entry
                .take((MAX_ARCHIVE_BYTES as u64) + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::UnsupportedArchive)?;
            if bytes.len() > MAX_ARCHIVE_BYTES {
                Err(Error::LimitExceeded)
            } else {
                Ok(Some(bytes))
            }
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(_) => Err(Error::UnsupportedArchive),
    }
}
