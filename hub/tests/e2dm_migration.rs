//! E2 migration safety, against a database written by the real v1.0.2 hub (the release before
//! end-to-end encryption). Same method as `dm_migration.rs`: `releases/v1.0.2/concord-server.exe`
//! (or `VOX_V102_HUB`) runs on a temp dir, two people exchange a DM with an image, it is killed like
//! a crash, and the new hub opens a *copy*: `attachments.sealed` is added (`0` for every old
//! image), the old lines and images read as before, sealed lines and images work (and survive a
//! restart), and the v1.0.2 hub still runs on the migrated copy (downgrade: it never reads the new
//! column, and a sealed line is only base64 to it, never plaintext). Skipped when the old binary is
//! not there.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{json, Value};
use vox_proto::*;
use voxhub_server::{start, Options};

const PASSWORD: &str = "pw";

fn old_hub() -> Option<PathBuf> {
    let p = std::env::var_os("VOX_V102_HUB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../releases/v1.0.2/concord-server.exe"));
    p.exists().then_some(p)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn config(dir: &Path, port: u16) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"mig\"\nserver_password = \"{PASSWORD}\"\nbind = \"127.0.0.1:{port}\"\nowner = \"Ana\"\n\
             [livekit]\napi_key = \"APImig000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

/// Runs the old hub; killed on drop (only this exact child).
struct OldHub(Child);

impl Drop for OldHub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn run_old(bin: &Path, dir: &Path, port: u16) -> OldHub {
    let child = Command::new(bin)
        .arg("--data-dir")
        .arg(dir)
        .arg("--no-livekit")
        .arg("--log-dir")
        .arg(dir.join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let hub = OldHub(child);
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "the old hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub
}

async fn login(http: &reqwest::Client, base: &str, name: &str) -> (String, i64) {
    let r = http.post(format!("{base}/api/login")).json(&json!({"username": name, "server_password": PASSWORD, "protocol_version": 1})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "login {name}");
    let v: Value = r.json().await.unwrap();
    (v["token"].as_str().unwrap().to_string(), v["user"]["id"].as_i64().unwrap())
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 3) as u8, (y * 9) as u8, 40]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn copy_db(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for name in ["voxhub.db", "voxhub.db-wal", "voxhub.db-shm"] {
        if from.join(name).exists() {
            std::fs::copy(from.join(name), to.join(name)).unwrap();
        }
    }
    // The stored images go with the database.
    let (src, dst) = (from.join("attachments"), to.join("attachments"));
    if let Ok(dirs) = std::fs::read_dir(&src) {
        for d in dirs.flatten() {
            let out = dst.join(d.file_name());
            std::fs::create_dir_all(&out).unwrap();
            for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                std::fs::copy(f.path(), out.join(f.file_name())).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn a_v102_database_gets_sealed_images_and_stays_usable() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v1.0.2 hub binary (set VOX_V102_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v102"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v1.0.2 hub writes its database: a DM with an image
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let (ana, _) = login(&http, &base, "Ana").await;
    let (bia, bia_id) = login(&http, &base, "Bia").await;
    let dm: Value = http.post(format!("{base}/api/dms/{bia_id}")).bearer_auth(&ana).json(&json!({})).send().await.unwrap().json().await.unwrap();
    let dm_id = dm["id"].as_i64().unwrap();
    let up: Value = http.post(format!("{base}/api/attachments?thumb_len=0")).bearer_auth(&ana).body(png(64, 48)).send().await.unwrap().json().await.unwrap();
    let old_img = up["id"].as_i64().unwrap();
    let r = http.post(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ana).json(&json!({"content": "foto do v1.0.2", "attachments": [old_img]})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let mut sealed_line = 0;
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        let lines: Vec<DmMessage> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&bia).send().await.unwrap().json().await.unwrap();
        let old_line = lines.iter().find(|m| m.sealed.is_none()).expect("the v1.0.2 line");
        assert_eq!(old_line.caption(), "foto do v1.0.2", "round {round}");
        assert_eq!(old_line.attachments.iter().map(|a| (a.id, a.sealed)).collect::<Vec<_>>(), vec![(old_img, false)]);
        assert!(old_line.sealed_attachments.is_empty());
        assert_eq!(http.get(format!("{base}/api/attachments/{old_img}")).bearer_auth(&bia).send().await.unwrap().status(), StatusCode::OK);
        if round == 0 {
            let r = http.post(format!("{base}/api/attachments?sealed=1&thumb_len=0")).bearer_auth(&ana).body(vec![0x5a; 777]).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let att: Attachment = r.json().await.unwrap();
            assert!(att.sealed);
            let r = http.post(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ana).json(&json!({"sealed": "AQIDBAUG", "attachments": [att.id]})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            sealed_line = r.json::<DmMessage>().await.unwrap().id;
        } else {
            let line = lines.iter().find(|m| m.id == sealed_line).expect("the sealed line survived the restart");
            assert_eq!((line.content.as_str(), line.sealed.as_deref(), line.sealed_attachments.len()), (DM_SEALED_PLACEHOLDER, Some("AQIDBAUG"), 1));
        }
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("SELECT sealed FROM attachments WHERE id = ?", [old_img], |r| r.get::<_, i64>(0))?, 0, "old images are readable ones");
        assert_eq!(c.query_row("SELECT COUNT(*) FROM attachments WHERE sealed = 1", [], |r| r.get::<_, i64>(0))?, 1);
        assert_eq!(c.query_row("SELECT format FROM dm_messages WHERE id = ?", [sealed_line], |r| r.get::<_, String>(0))?, "e2e1");
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade: the v1.0.2 hub still runs on the migrated copy
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let lines: Vec<Value> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&bia).send().await.unwrap().json().await.unwrap();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().any(|m| m["content"].as_str().is_some_and(|c| c.starts_with("foto do v1.0.2"))));
    let r = http.post(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&bia).json(&json!({"content": "de volta ao v1.0.2"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    drop(old);
}
