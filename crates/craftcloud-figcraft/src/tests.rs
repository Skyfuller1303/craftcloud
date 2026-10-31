use std::{
    fs,
    io::Write,
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use figcraft_collab::message::{Admin, AdminReply, ServerMsg};
use figcraft_doc::{
    Document, ImageData, NodeKind,
    color::{ImageScaleMode, Paint},
};

use crate::{
    Access, Backend, Client, Error, Link,
    client::{
        MAX_MESSAGES, OriginalArchive, archive_metadata, connect_socket, known_archive_entry,
        operation_deadline, read_server_msg, resolve_addresses_with, send_client, verify_export,
    },
};

fn fixture() -> (Vec<u8>, figcraft_doc::NodeId, String, Vec<u8>) {
    let png = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240, 31,
        0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let hash = figcraft_format::image_hash(&png);
    let mut doc = Document::new("Synthetic adapter fixture");
    let Some(page) = doc.first_page() else {
        return (Vec::new(), figcraft_doc::NodeId(0), hash, png);
    };
    let Ok(shape) = doc.create(
        page,
        NodeKind::Rectangle,
        "Fixture image",
        4.0,
        8.0,
        80.0,
        60.0,
    ) else {
        return (Vec::new(), figcraft_doc::NodeId(0), hash, png);
    };
    doc.images.insert(
        hash.clone(),
        ImageData {
            bytes: Arc::new(png.clone()),
            width: 1,
            height: 1,
        },
    );
    if let Some(node) = doc.node_mut(shape) {
        node.fills
            .push(Paint::image(hash.clone(), ImageScaleMode::Fit));
    }
    let version =
        figcraft_format::Version::new("v1", "Initial", "synthetic", 1, Arc::new(doc.clone()));
    let bytes = figcraft_format::save_bytes_with_history(&doc, Some(&png), &[version], &[])
        .unwrap_or_default();
    (bytes, shape, hash, png)
}

#[test]
fn link_debug_never_discloses_token() {
    let link = Link {
        id: "0123456789abcdef".into(),
        token: "synthetic-secret-token".into(),
    };
    let debug = format!("{link:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("synthetic-secret-token"));
}

#[test]
fn client_accepts_only_credential_free_ws_endpoints() {
    assert!(matches!(
        Client::new("http://127.0.0.1:47900/", "abc".into()),
        Err(Error::InvalidEndpoint)
    ));
    assert!(matches!(
        Client::new("ws://user:pass@localhost/", "abc".into()),
        Err(Error::InvalidEndpoint)
    ));
    assert!(matches!(
        Client::new("ws://localhost/?key=secret", "abc".into()),
        Err(Error::InvalidEndpoint)
    ));
    assert!(matches!(
        Client::new("ws://localhost/figcraft/", String::new()),
        Err(Error::InvalidKey)
    ));
    assert!(Client::new("ws://localhost/figcraft/", "server_key_123".into()).is_ok());
}

#[test]
fn native_archive_and_archive_only_metadata_roundtrip_without_loss()
-> Result<(), Box<dyn std::error::Error>> {
    let (bytes, shape, hash, png) = fixture();
    assert!(!bytes.is_empty());
    let (loaded, thumbnail) = archive_metadata(&bytes)?;
    assert!(loaded.issues.is_empty());
    assert_eq!(loaded.versions.len(), 1);
    assert_eq!(thumbnail.as_deref(), Some(png.as_slice()));
    let node = loaded.doc.node(shape).ok_or("fixture node must load")?;
    assert_eq!(node.name, "Fixture image");
    assert_eq!(
        loaded.doc.images.get(&hash).map(|img| img.bytes.as_slice()),
        Some(png.as_slice())
    );

    let source = OriginalArchive {
        doc: loaded.doc.clone(),
        versions: loaded.versions,
        branches: loaded.branches,
        thumbnail,
    };
    let export = figcraft_format::save_bytes_with_history(
        &source.doc,
        source.thumbnail.as_deref(),
        &source.versions,
        &source.branches,
    )?;
    assert!(!export.is_empty());
    assert!(verify_export(&source.doc, &source, &export).is_ok());
    Ok(())
}

#[test]
fn unknown_archive_members_are_refused_as_unsupported() {
    assert!(known_archive_entry("images/0123456789abcdef"));
    assert!(known_archive_entry("versions/v1.json"));
    assert!(known_archive_entry("branches/b1/head.json"));
    assert!(known_archive_entry("library/manifest.json"));
    assert!(!known_archive_entry("provider/private-metadata.json"));
    assert!(!known_archive_entry("../outside.json"));
}

#[test]
fn access_labels_are_native_server_levels() {
    assert_eq!(Access::Edit.as_str(), "edit");
    assert_eq!(Access::View.as_str(), "view");
}

#[test]
fn partial_websocket_frame_is_stopped_by_absolute_deadline()
-> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let peer = thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let Ok(mut websocket) = tungstenite::accept(stream) else {
            return;
        };
        let Ok(mut stream) = websocket.get_mut().try_clone() else {
            return;
        };
        let header = [0x82, 127, 0, 0, 0, 0, 4, 0, 0, 0];
        if stream.write_all(&header).is_err() {
            return;
        }
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(40));
            if stream.write_all(&[0]).is_err() {
                return;
            }
        }
    });
    let endpoint = url::Url::parse(&format!("ws://{address}/"))?;
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    let mut socket = connect_socket(&endpoint, deadline)?;
    let started = std::time::Instant::now();
    assert!(read_server_msg(&mut socket).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(socket);
    let _ = peer.join();
    Ok(())
}

#[test]
fn stalled_dns_returns_by_deadline_and_allows_only_one_resolver_thread()
-> Result<(), Box<dyn std::error::Error>> {
    let starts = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let worker_starts = Arc::clone(&starts);
    let worker_active = Arc::clone(&active);
    let started = std::time::Instant::now();
    let first = resolve_addresses_with(
        "stalled.example.invalid",
        443,
        started + Duration::from_millis(100),
        move |_, port| {
            worker_starts.fetch_add(1, Ordering::SeqCst);
            worker_active.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(350));
            worker_active.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
            )])
        },
    );
    assert!(first.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    thread::sleep(Duration::from_millis(30));
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(active.load(Ordering::SeqCst), 1);
    let second_starts = Arc::clone(&starts);
    let second = resolve_addresses_with(
        "another.example.invalid",
        443,
        std::time::Instant::now() + Duration::from_millis(100),
        move |_, _| {
            second_starts.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        },
    );
    assert!(second.is_err());
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    for _ in 0..50 {
        if active.load(Ordering::SeqCst) == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let resolved = resolve_addresses_with(
        "127.0.0.1",
        443,
        std::time::Instant::now() + Duration::from_secs(1),
        |_, _| Err(()),
    )?;
    assert_eq!(
        resolved,
        vec!["127.0.0.1:443".parse::<std::net::SocketAddr>()?]
    );

    let active = Arc::new(AtomicUsize::new(0));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let worker_active = Arc::clone(&active);
    let worker_overlaps = Arc::clone(&overlaps);
    let (ready_send, ready_receive) = std::sync::mpsc::sync_channel(1);
    let first_caller = thread::spawn(move || {
        resolve_addresses_with(
            "healthy-first.example.invalid",
            443,
            std::time::Instant::now() + Duration::from_secs(1),
            move |_, port| {
                if worker_active.fetch_add(1, Ordering::SeqCst) != 0 {
                    worker_overlaps.fetch_add(1, Ordering::SeqCst);
                }
                let _ = ready_send.send(());
                thread::sleep(Duration::from_millis(150));
                worker_active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    port,
                )])
            },
        )
    });
    ready_receive.recv_timeout(Duration::from_secs(1))?;
    let queued_at = std::time::Instant::now();
    let second = resolve_addresses_with(
        "healthy-second.example.invalid",
        443,
        std::time::Instant::now() + Duration::from_secs(1),
        |_, port| {
            Ok(vec![std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
            )])
        },
    );
    assert!(queued_at.elapsed() >= Duration::from_millis(100));
    let first = first_caller
        .join()
        .map_err(|_| "resolver caller thread panicked")?;
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(overlaps.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
#[ignore = "requires CC_TEST_FIGCRAFT_SERVER to name the native server executable"]
fn native_server_publish_link_export_revoke_remove() -> Result<(), Box<dyn std::error::Error>> {
    let exe =
        std::env::var_os("CC_TEST_FIGCRAFT_SERVER").ok_or("missing CC_TEST_FIGCRAFT_SERVER")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let root =
        std::env::temp_dir().join(format!("craftcloud-figcraft-test-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let data = root.join("data");
    let mut child = Command::new(exe)
        .args(["--listen", &address.to_string(), "--public-url"])
        .arg(format!("ws://{address}/figcraft/"))
        .arg("--data")
        .arg(&data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut ready = false;
        for _ in 0..100 {
            if child.try_wait()?.is_some() {
                return Err("native server exited before listening".into());
            }
            if TcpStream::connect(address).is_ok() {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            return Err("native server startup timed out".into());
        }
        let key = fs::read_to_string(data.join("server.key"))?
            .trim()
            .to_owned();
        let client = Client::new(&format!("ws://{address}/figcraft/"), key.clone())?;
        let (bytes, shape, _, _) = fixture();
        let file = client.publish(&bytes, "ignored test name")?;
        let edit = client.create_link(&file, Access::Edit)?;
        let view = client.create_link(&file, Access::View)?;
        let deadline = operation_deadline();
        let label = client.export_link_label(&file);
        let orphan = client.create_link_inner(&file, Access::View, &label, deadline)?;
        let dead_listener = TcpListener::bind("127.0.0.1:0")?;
        let dead_address = dead_listener.local_addr()?;
        drop(dead_listener);
        let disconnected = Client::new(&format!("ws://{dead_address}/"), key)?;
        assert!(disconnected.revoke_link(&file, &orphan.id).is_err());
        drop(orphan);
        let (mut socket, mut session) = client.join_snapshot(&file, &edit.token, deadline)?;
        let mut changed = session.document().clone();
        let node = changed
            .node_mut(shape)
            .ok_or("fixture node missing from live snapshot")?;
        node.name = "Edited by adapter integration test".to_owned();
        let batch = session
            .edit(&changed)?
            .ok_or("edit produced no protocol message")?;
        send_client(&mut socket, batch)?;
        let mut accepted = false;
        for _ in 0..MAX_MESSAGES {
            let message = read_server_msg(&mut socket)?;
            if matches!(&message, ServerMsg::Changes(_)) {
                session.receive(message, &figcraft_collab::NoRecompute)?;
                accepted = true;
                break;
            }
        }
        if !accepted {
            return Err("server did not acknowledge live edit".into());
        }
        let exported = client.export(&file, &bytes)?;
        let (loaded, _) = archive_metadata(&exported.bytes)?;
        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.doc.images.len(), 1);
        assert_eq!(
            loaded.doc.node(shape).map(|node| node.name.as_str()),
            Some("Edited by adapter integration test")
        );
        let links =
            client.request_admin(Admin::Links { file: file.clone() }, operation_deadline())?;
        let AdminReply::Links(links) = links else {
            return Err("link listing returned unexpected response".into());
        };
        assert!(
            !links
                .iter()
                .any(|link| link.file == file && link.label == label)
        );
        client.revoke_link(&file, &edit.id)?;
        client.revoke_link(&file, &view.id)?;
        client.remove(&file)?;
        client.remove(&file)?;
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(root);
    result
}
