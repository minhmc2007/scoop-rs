//! End-to-end integration test: runs the full install -> list -> status ->
//! update -> uninstall pipeline against a local HTTP server, entirely
//! in-process and network-free (except localhost).

use scoop_rs::config::Root;
use scoop_rs::db::Db;
use scoop_rs::install::{install_specs, update_one, Ctx, InstallOptions};
use scoop_rs::util::Output;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

struct TestServer {
    handle: Option<std::thread::JoinHandle<()>>,
    url: String,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // wake the accept loop with a dummy connection
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(payload: Vec<u8>) -> (TestServer, String) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind test server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("test server must use an IP socket")
        .port();
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_t = Arc::clone(&shutdown);
    let payload = Arc::new(payload);
    let handle = std::thread::spawn(move || loop {
        if shutdown_t.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        match server.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(Some(request)) => {
                let body = (*payload).clone();
                let mut resp = tiny_http::Response::from_data(body);
                resp.add_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/zip"[..])
                        .unwrap(),
                );
                let _ = request.respond(resp);
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    });
    let url = format!("http://127.0.0.1:{port}");
    (
        TestServer {
            handle: Some(handle),
            url: url.clone(),
            shutdown,
        },
        url,
    )
}

fn build_test_zip() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zw.start_file("bin/fake.exe", opts).unwrap();
        zw.write_all(b"#!/bin/sh\necho fake binary\n").unwrap();
        zw.start_file("bin/tool.exe", opts).unwrap();
        zw.write_all(b"#!/bin/sh\necho tool\n").unwrap();
        zw.start_file("config/settings.ini", opts).unwrap();
        zw.write_all(b"key=value\n").unwrap();
        zw.finish().unwrap();
    }
    buf.into_inner()
}

struct Env {
    _dir: tempfile::TempDir,
    root: Root,
    db: Db,
    out: Output,
    opts: InstallOptions,
}

fn setup() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Root::from_path(dir.path().join("scoop"));
    root.ensure_layout().unwrap();
    let db = Db::open(&root.db_file()).unwrap();

    Env {
        _dir: dir,
        root,
        db,
        out: Output { quiet: true },
        opts: InstallOptions {
            force: false,
            skip_deps: false,
            no_progress: true,
        },
    }
}

fn add_local_bucket(env: &Env, name: &str, manifest: &str, app: &str) {
    let bdir = env.root.bucket_dir(name);
    std::fs::create_dir_all(&bdir).unwrap();
    std::fs::write(bdir.join(format!("{app}.json")), manifest).unwrap();
    scoop_rs::buckets::index_bucket(&env.root, &env.db, name).unwrap();
    env.db
        .upsert_bucket(&scoop_rs::db::BucketRow {
            name: name.to_string(),
            url: "https://example.invalid/local".into(),
            commit: None,
            last_sync_ms: Some(0),
            manifest_count: 1,
        })
        .unwrap();
}

#[test]
fn full_lifecycle() {
    let env = setup();
    let zip_bytes = build_test_zip();

    let sha = scoop_rs::util::sha256_hex(&zip_bytes);
    let (_server, url) = serve(zip_bytes.clone());
    let manifest = format!(
        r#"{{
            "version": "1.0.0",
            "description": "integration test app",
            "url": "{url}/fake-1.0.0.zip",
            "hash": "{sha}",
            "bin": [ [ "bin/fake.exe", "fake" ], "bin/tool.exe" ],
            "persist": [ "config" ],
            "env_add_path": "bin"
        }}"#
    );
    add_local_bucket(&env, "test", &manifest, "fakeapp");

    // ---- install ----
    let ctx = Ctx {
        root: &env.root,
        db: &env.db,
        out: &env.out,
        opts: &env.opts,
    };
    let installed = install_specs(&ctx, &["fakeapp".to_string()]).unwrap();
    assert_eq!(installed, vec!["fakeapp"]);

    let vdir = env.root.version_dir("fakeapp", "1.0.0");
    assert!(
        vdir.join("bin/fake.exe").is_file(),
        "extracted file must exist"
    );
    assert!(
        vdir.join("config/settings.ini").is_file(),
        "extracted config must exist"
    );
    assert!(vdir.join("install.json").is_file());

    // current link
    let current = env.root.current_dir("fakeapp");
    assert!(current.exists(), "current link must exist");

    // shims
    let shim = env.root.shims().join("fake.cmd");
    assert!(shim.is_file(), "shim cmd must exist: {}", shim.display());
    let shim_body = std::fs::read_to_string(&shim).unwrap();
    assert!(shim_body.contains("bin/fake.exe") || shim_body.contains("bin\\fake.exe"));

    // persist: config moved to persist dir and linked back
    let persist_cfg = env.root.persist_dir("fakeapp").join("config");
    assert!(persist_cfg.is_dir(), "persist dir must exist");
    assert!(persist_cfg.join("settings.ini").is_file());
    assert!(
        current.join("config/settings.ini").is_file(),
        "persist linked back into install"
    );

    // DB state
    let pkg = env.db.get_package("fakeapp").unwrap().expect("db row");
    assert_eq!(pkg.version, "1.0.0");
    assert_eq!(pkg.bucket.as_deref(), Some("test"));
    let files = env.db.files_of("fakeapp").unwrap();
    assert!(files.iter().any(|f| f.contains("bin/fake.exe")));
    assert_eq!(env.db.shims_of("fakeapp").unwrap().len(), 2);

    // ---- cache reuse on reinstall (same file served once more, no error) ----
    drop(_server);
    let (_server2, url2) = serve(zip_bytes.clone());
    let manifest2 = manifest.replace(&url, &url2);
    std::fs::write(env.root.bucket_dir("test").join("fakeapp.json"), manifest2).unwrap();
    scoop_rs::buckets::index_bucket(&env.root, &env.db, "test").unwrap();

    // ---- status: up to date ----
    let rows = scoop_rs::install::compute_status(&ctx).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].4, "same version must not be updatable");

    // ---- update to 1.1.0 ----
    let sha2 = scoop_rs::util::sha256_hex(&zip_bytes);
    let manifest3 = format!(
        r#"{{
            "version": "1.1.0",
            "url": "{url2}/fake-1.1.0.zip",
            "hash": "{sha2}",
            "bin": [ [ "bin/fake.exe", "fake" ], "bin/tool.exe" ],
            "persist": [ "config" ]
        }}"#
    );
    std::fs::write(env.root.bucket_dir("test").join("fakeapp.json"), manifest3).unwrap();
    scoop_rs::buckets::index_bucket(&env.root, &env.db, "test").unwrap();

    let rows = scoop_rs::install::compute_status(&ctx).unwrap();
    assert!(rows[0].4, "1.1.0 must be detected as update");

    let updated = update_one(&ctx, "fakeapp").unwrap();
    assert_eq!(updated.as_deref(), Some("1.1.0"));

    let newvdir = env.root.version_dir("fakeapp", "1.1.0");
    assert!(newvdir.join("bin/fake.exe").is_file());
    assert!(
        !vdir.exists(),
        "old version dir must be removed after update"
    );
    assert!(env
        .root
        .persist_dir("fakeapp")
        .join("config/settings.ini")
        .is_file());
    let pkg = env.db.get_package("fakeapp").unwrap().unwrap();
    assert_eq!(pkg.version, "1.1.0");

    // user edits persist across the update
    let cfg = env.root.persist_dir("fakeapp").join("config/settings.ini");
    std::fs::write(&cfg, b"key=edited").unwrap();
    assert_eq!(
        std::fs::read(current.join("config/settings.ini")).unwrap(),
        b"key=edited"
    );

    // ---- uninstall ----
    let uopts = scoop_rs::install::InstallOptions {
        force: false,
        skip_deps: true,
        no_progress: true,
    };
    scoop_rs::uninstall::uninstall_apps(
        &env.root,
        &env.db,
        &env.out,
        &uopts,
        &["fakeapp".to_string()],
        false,
    )
    .unwrap();
    assert!(
        env.db.get_package("fakeapp").unwrap().is_none(),
        "db row deleted"
    );
    assert!(!env.root.app_dir("fakeapp").exists(), "app dir removed");
    assert!(!env.root.shims().join("fake.cmd").exists(), "shims removed");
    assert!(
        env.root.persist_dir("fakeapp").is_dir(),
        "persist kept without --purge"
    );
    assert_eq!(std::fs::read_to_string(cfg).unwrap(), "key=edited");
}

#[test]
fn hash_mismatch_aborts_install() {
    let env = setup();
    let zip_bytes = build_test_zip();
    let (_server, url) = serve(zip_bytes);
    let manifest = format!(
        r#"{{ "version": "1.0.0", "url": "{url}/x.zip", "hash": "{}" }}"#,
        "0".repeat(64)
    );
    add_local_bucket(&env, "test", &manifest, "badapp");
    let ctx = Ctx {
        root: &env.root,
        db: &env.db,
        out: &env.out,
        opts: &env.opts,
    };
    let err = install_specs(&ctx, &["badapp".to_string()]).unwrap_err();
    assert!(err.to_string().contains("hash mismatch"), "got: {err}");
    assert!(!env
        .root
        .app_dir("badapp")
        .join("1.0.0")
        .join("install.json")
        .exists());
}

#[test]
fn dependency_is_installed_first() {
    let env = setup();
    let zip_bytes = build_test_zip();
    let sha = scoop_rs::util::sha256_hex(&zip_bytes);
    let (_server, url) = serve(zip_bytes);

    let lib_manifest = format!(
        r#"{{ "version": "0.9.0", "url": "{url}/lib.zip", "hash": "{sha}", "bin": "bin/tool.exe" }}"#
    );
    let app_manifest = format!(
        r#"{{ "version": "2.0.0", "url": "{url}/app.zip", "hash": "{sha}", "depends": [ "lib" ], "bin": "bin/fake.exe" }}"#
    );
    add_local_bucket(&env, "test", &lib_manifest, "lib");
    add_local_bucket(&env, "test", &app_manifest, "needslib");

    let ctx = Ctx {
        root: &env.root,
        db: &env.db,
        out: &env.out,
        opts: &env.opts,
    };
    install_specs(&ctx, &["needslib".to_string()]).unwrap();
    assert!(env.db.is_installed("lib").unwrap());
    assert!(env.db.is_installed("needslib").unwrap());
}

#[test]
fn local_manifest_path_installs() {
    let env = setup();
    let zip_bytes = build_test_zip();
    let sha = scoop_rs::util::sha256_hex(&zip_bytes);
    let (_server, url) = serve(zip_bytes);
    let manifest_path: PathBuf = env._dir.path().join("local-manifest.json");
    std::fs::write(
        &manifest_path,
        format!(r#"{{ "version": "3.2.1", "url": "{url}/l.zip", "hash": "{sha}", "bin": "bin/fake.exe" }}"#),
    )
    .unwrap();
    let ctx = Ctx {
        root: &env.root,
        db: &env.db,
        out: &env.out,
        opts: &env.opts,
    };
    install_specs(&ctx, &[manifest_path.display().to_string()]).unwrap();
    assert!(env.db.is_installed("local-manifest").unwrap());
    assert_eq!(
        env.db
            .get_package("local-manifest")
            .unwrap()
            .unwrap()
            .bucket
            .as_deref(),
        Some("local")
    );
}

#[test]
fn search_finds_indexed_apps() {
    let env = setup();
    add_local_bucket(
        &env,
        "test",
        r#"{ "version": "1.0", "description": "fancy downloader", "url": "https://x/y.zip" }"#,
        "fancyapp",
    );
    let hits = env.db.search_index_entries("fancy", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "fancyapp");
    let none = env.db.search_index_entries("zzzznotfound", 10).unwrap();
    assert!(none.is_empty());
}

#[test]
fn import_existing_scoop_install() {
    let env = setup();
    // simulate a PS-scoop-installed app: apps/<app>/current/install.json
    let vdir = env.root.version_dir("legacy", "5.5");
    std::fs::create_dir_all(vdir.join("bin")).unwrap();
    std::fs::write(vdir.join("bin/legacy.exe"), b"MZ").unwrap();
    std::fs::write(
        vdir.join("install.json"),
        r#"{ "version": "5.5", "bin": "bin/legacy.exe" }"#,
    )
    .unwrap();
    let current = env.root.current_dir("legacy");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&vdir, &current).unwrap();
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&current)
            .arg(&vdir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    let imported = scoop_rs::install::import_existing(&env.root, &env.db, &env.out).unwrap();
    assert_eq!(imported, 1);
    let pkg = env.db.get_package("legacy").unwrap().unwrap();
    assert_eq!(pkg.version, "5.5");
    assert_eq!(pkg.bucket.as_deref(), Some("(scoop)"));
}
