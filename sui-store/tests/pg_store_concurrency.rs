//! Concurrency + write-if-absent proofs for `PgStore` — the durable,
//! content-addressed [`Store`] whose blob key IS `GraphHash::of(bytes)` and
//! whose metadata-row + blob land in ONE transaction (`upsert_path_atomic`).
//!
//! The crate's own unit tests (in `src/pg.rs`) prove the SEQUENTIAL
//! content-addressing + atomicity + integrity core against the in-memory
//! oracle. These integration tests add:
//!
//! 1. **Same-content race → one path, one blob, all readers agree (§1).** N
//!    tokio tasks race to `add_to_store` the SAME NAR bytes. Because the store
//!    path AND the content key are both derived from the bytes, every writer
//!    computes the identical path/key — so the race is degenerate: exactly one
//!    metadata row, exactly one blob, and every reader reads back the correct
//!    validated bytes. No torn/partial write, last-writer-safe (idempotent).
//!
//! 2. **Distinct-content race → all land, no cross-contamination (§2).** N tasks
//!    each `add_to_store` DIFFERENT bytes concurrently → N rows + N blobs, and
//!    each content key resolves to ITS OWN validated bytes.
//!
//! 3. **Concurrent readers during writes never observe a half-applied path
//!    (§3).** A reader pool races the writer pool; a path that `is_valid_path`
//!    reports present must have its blob present + validating (the atomic-write
//!    invariant: never a metadata row without its blob).
//!
//! All built on the SHIPPED public surface (`PgStore`, `InMemoryPgBackend`,
//! `Store`, `get_validated_blob`). A live-Postgres proof against the real
//! `SqlxPgBackend` transport lives in the `#[ignore]`d `real_infra` test.

use std::sync::Arc;

use sui_compat::store_path::StorePath;
use sui_graph_store::GraphHash;
use sui_store::{InMemoryPgBackend, PgStore, Store};

/// A generous async racer count.
const RACERS: usize = 32;

fn store() -> Arc<PgStore<InMemoryPgBackend>> {
    Arc::new(PgStore::new(InMemoryPgBackend::new()))
}

// ═══════════════════════════════════════════════════════════════════════════
// §1  same-content race → exactly one path + one blob, all readers agree
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_add_to_store_same_bytes_yields_one_path_one_blob() {
    let s = store();
    let nar: Arc<Vec<u8>> = Arc::new(b"the one true durable NAR payload".to_vec());
    let key = *GraphHash::of(&nar).as_bytes();

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let s = s.clone();
        let nar = Arc::clone(&nar);
        set.spawn(async move { s.add_to_store("hello-2.12.1", &nar, &[]).await });
    }

    // Collect every racer's returned path — they must ALL be identical
    // (content-addressed ⇒ one path).
    let mut paths = Vec::with_capacity(RACERS);
    while let Some(res) = set.join_next().await {
        let info = res.expect("task panicked").expect("racing add_to_store must succeed");
        paths.push(info.path);
    }
    assert_eq!(paths.len(), RACERS);
    let first = &paths[0];
    assert!(paths.iter().all(|p| p == first), "content-address ⇒ every racer returns the same path");

    // Exactly ONE metadata row and ONE blob — the race collapsed.
    assert_eq!(s.backend().path_count(), 1, "same-content race must collapse to one row");
    assert_eq!(s.backend().blob_count(), 1, "same-content race must collapse to one blob");

    // Every reader reads back the correct, validated bytes.
    let bytes = s.get_validated_blob(&key).await.unwrap().unwrap();
    assert_eq!(bytes, *nar, "the one stored blob is the correct content");

    // Concurrent validated reads all agree.
    let mut reads = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let s = s.clone();
        reads.spawn(async move { s.get_validated_blob(&key).await });
    }
    while let Some(res) = reads.join_next().await {
        let got = res.expect("reader panicked").expect("validated read ok").expect("blob present");
        assert_eq!(got, *nar, "every concurrent reader agrees on the canonical bytes");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// §2  distinct-content race → all land, no cross-contamination
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_add_to_store_distinct_bytes_all_land_without_cross_contamination() {
    let s = store();

    // Each task owns unique bytes → a unique content key + unique path.
    let payloads: Vec<Vec<u8>> =
        (0..RACERS).map(|i| format!("distinct-nar-#{i}-{}", "z".repeat(i)).into_bytes()).collect();

    let mut set = tokio::task::JoinSet::new();
    for (i, p) in payloads.iter().cloned().enumerate() {
        let s = s.clone();
        set.spawn(async move {
            let info = s.add_to_store(&format!("pkg-{i}"), &p, &[]).await?;
            Ok::<_, sui_store::StoreError>((info.path, p))
        });
    }

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res.expect("task panicked").expect("distinct add ok"));
    }

    // N distinct rows + N distinct blobs.
    assert_eq!(s.backend().path_count(), RACERS);
    assert_eq!(s.backend().blob_count(), RACERS);

    // Each path resolves to a valid PathInfo, and each content key returns ITS
    // OWN validated bytes (no cross-contamination).
    for (path, bytes) in &results {
        let sp = StorePath::from_absolute_path(path).unwrap();
        assert!(s.is_valid_path(&sp).await.unwrap(), "each distinct path must be valid");
        let key = *GraphHash::of(bytes).as_bytes();
        let got = s.get_validated_blob(&key).await.unwrap().unwrap();
        assert_eq!(&got, bytes, "a content key returned another key's bytes");
    }

    // Every returned path is unique.
    let mut paths: Vec<&String> = results.iter().map(|(p, _)| p).collect();
    paths.sort();
    let unique = {
        let mut u = paths.clone();
        u.dedup();
        u.len()
    };
    assert_eq!(unique, RACERS, "distinct content must yield distinct paths");
}

// ═══════════════════════════════════════════════════════════════════════════
// §3  concurrent readers during writes never see a half-applied path
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_never_observe_a_metadata_row_without_its_blob() {
    // The atomicity invariant: `add_to_store` writes the row AND the blob in one
    // transaction. So any path a reader sees as valid MUST have a present,
    // validating blob — a reader can never catch a half-applied write.
    let s = store();
    let nar: Arc<Vec<u8>> = Arc::new(vec![0x5Au8; 32 * 1024]);
    let key = *GraphHash::of(&nar).as_bytes();

    let mut set = tokio::task::JoinSet::new();
    // Writers.
    for _ in 0..RACERS {
        let s = s.clone();
        let nar = Arc::clone(&nar);
        set.spawn(async move {
            let _ = s.add_to_store("pkg", &nar, &[]).await?;
            Ok::<(), sui_store::StoreError>(())
        });
    }
    // Readers: as soon as any path is valid, its blob must be present+valid.
    for _ in 0..RACERS {
        let s = s.clone();
        let nar = Arc::clone(&nar);
        set.spawn(async move {
            for _ in 0..500 {
                if let Some(sp) = s.query_all_valid_paths().await?.into_iter().next() {
                    // The path exists → the row exists → the blob MUST validate.
                    assert!(s.is_valid_path(&sp).await?);
                    let bytes = s
                        .get_validated_blob(&key)
                        .await?
                        .expect("atomic write ⇒ a valid path has its blob present");
                    assert_eq!(&bytes, &*nar, "blob must be the whole canonical value");
                    return Ok::<(), sui_store::StoreError>(());
                }
                tokio::task::yield_now().await;
            }
            Ok(())
        });
    }
    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("no half-applied path observed");
    }
    assert_eq!(s.backend().path_count(), 1);
    assert_eq!(s.backend().blob_count(), 1);
}

// ── register_path (metadata-only) races are last-writer-safe ───────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_register_path_same_path_is_idempotent() {
    use sui_store::PathInfo;
    let s = store();
    let abs = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
    let info = PathInfo {
        references: vec![],
        ..PathInfo::new(abs, "sha256:aaa")
    };

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let s = s.clone();
        let info = info.clone();
        set.spawn(async move { s.register_path(&info).await });
    }
    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("racing register_path must succeed");
    }
    // Idempotent: one row regardless of how many racers registered it.
    assert_eq!(s.backend().path_count(), 1);
    let sp = StorePath::from_absolute_path(abs).unwrap();
    assert!(s.is_valid_path(&sp).await.unwrap());
}

// ═══════════════════════════════════════════════════════════════════════════
// REAL-INFRA proof — a live Postgres via the shipped SqlxPgBackend transport.
// #[ignore]d: needs a docker daemon. Spins a throwaway postgres, runs the
// content-addressed atomic write races against it, and MUST pass when run:
//
//   cargo test -p sui-store --features postgres \
//     --test pg_store_concurrency -- --ignored --nocapture
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "postgres")]
mod real_infra {
    use super::*;
    use sui_store::pg::SqlxPgBackend;

    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("version")
            .arg("--format")
            .arg("{{.Server.Version}}")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn docker_run(args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("docker")
            .arg("run")
            .arg("-d")
            .arg("--rm")
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("docker run failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn docker_rm(id: &str) {
        let _ = std::process::Command::new("docker").arg("rm").arg("-f").arg(id).output();
    }

    async fn wait_port(addr: &str, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real docker daemon; spins a throwaway postgres. Run: cargo test -p sui-store --features postgres --test pg_store_concurrency -- --ignored --nocapture"]
    async fn live_postgres_content_addressed_atomic_write_races_are_safe() {
        if !docker_available() {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }
        let pg_port = 5441u16;
        let pg_id = docker_run(&[
            "-p",
            &format!("{pg_port}:5432"),
            "-e",
            "POSTGRES_PASSWORD=sui",
            "-e",
            "POSTGRES_USER=sui",
            "-e",
            "POSTGRES_DB=sui",
            "postgres:16-alpine",
        ])
        .expect("start postgres");

        struct Guard(String);
        impl Drop for Guard {
            fn drop(&mut self) {
                docker_rm(&self.0);
            }
        }
        let _guard = Guard(pg_id.clone());

        assert!(wait_port(&format!("127.0.0.1:{pg_port}"), 30).await, "postgres never came up");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let backend = SqlxPgBackend::connect(
            &format!("postgres://sui:sui@127.0.0.1:{pg_port}/sui"),
            8,
        )
        .await
        .expect("connect postgres");
        backend.migrate().await.expect("apply DDL");
        let s = Arc::new(PgStore::new(backend));

        // Same-content race against LIVE postgres → one path, one blob.
        let nar: Arc<Vec<u8>> = Arc::new(b"live durable NAR bytes".to_vec());
        let key = *GraphHash::of(&nar).as_bytes();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..24 {
            let s = s.clone();
            let nar = Arc::clone(&nar);
            set.spawn(async move { s.add_to_store("hello", &nar, &[]).await });
        }
        let mut paths = Vec::new();
        while let Some(r) = set.join_next().await {
            paths.push(r.expect("panicked").expect("live add ok").path);
        }
        let first = paths[0].clone();
        assert!(paths.iter().all(|p| *p == first), "content-address ⇒ one path on live PG");

        // Read back the validated blob from live postgres.
        let bytes = s.get_validated_blob(&key).await.unwrap().unwrap();
        assert_eq!(bytes, *nar);

        // Distinct-content race → each key its own validated bytes.
        let payloads: Vec<Vec<u8>> =
            (0..24).map(|i| format!("live-distinct-{i}").into_bytes()).collect();
        let mut set = tokio::task::JoinSet::new();
        for (i, p) in payloads.iter().cloned().enumerate() {
            let s = s.clone();
            set.spawn(async move {
                let info = s.add_to_store(&format!("pkg-{i}"), &p, &[]).await?;
                Ok::<_, sui_store::StoreError>((info.path, p))
            });
        }
        let mut results = Vec::new();
        while let Some(r) = set.join_next().await {
            results.push(r.expect("panicked").expect("live distinct ok"));
        }
        for (_, bytes) in &results {
            let k = *GraphHash::of(bytes).as_bytes();
            assert_eq!(&s.get_validated_blob(&k).await.unwrap().unwrap(), bytes);
        }

        // Integrity sweep on the live store passes.
        let vr = s.verify_store().await.unwrap();
        assert!(vr.corrupt.is_empty(), "live verify_store found corruption: {:?}", vr.corrupt);
        assert_eq!(vr.valid_count, vr.total_checked);
    }
}
