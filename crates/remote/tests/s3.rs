//! S3 integration tests against LocalStack or an S3-compatible service:
//!
//! ```text
//! docker compose -f docker/compose.yml up -d --wait localstack
//! SWITCHYARD_S3_ENDPOINT=http://127.0.0.1:4566 \
//!   cargo test -p switchyard-remote --test s3 -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::time::Duration;

use switchyard_remote::{AwsCredentials, EntryKind, RemoteFs, S3Config, S3Fs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn endpoint() -> String {
    std::env::var("SWITCHYARD_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4566".into())
}

fn test_config(bucket: Option<&str>) -> S3Config {
    S3Config {
        region: "us-east-1".into(),
        bucket: bucket.map(|s| s.to_string()),
        endpoint: Some(endpoint()),
        force_path_style: true,
        credentials: AwsCredentials {
            access_key_id: "test".into(),
            secret_access_key: "test".into(),
            session_token: None,
        },
    }
}

/// Wait until LocalStack S3 is reachable.
async fn wait_for_s3(fs: &S3Fs) {
    for _ in 0..30 {
        if fs.list(Path::new("/")).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("S3 endpoint not reachable at {}", endpoint());
}

#[tokio::test]
#[ignore = "needs localstack (docker compose up -d localstack)"]
async fn test_s3_root_bucket_crud() {
    let fs = S3Fs::new(test_config(None));
    wait_for_s3(&fs).await;

    let bucket_name = format!("test-bucket-{}", std::process::id());
    let bucket_path = Path::new("/").join(&bucket_name);

    // Create bucket via mkdir at root
    fs.mkdir(&bucket_path).await.expect("create bucket");

    // List buckets and verify
    let buckets = fs.list(Path::new("/")).await.expect("list buckets");
    assert!(
        buckets
            .iter()
            .any(|e| e.name == bucket_name && e.kind == EntryKind::Dir),
        "newly created bucket should be in root listing"
    );

    // Delete bucket
    fs.delete(&bucket_path).await.expect("delete bucket");

    // Verify it is gone
    let buckets_after = fs
        .list(Path::new("/"))
        .await
        .expect("list buckets after delete");
    assert!(
        !buckets_after.iter().any(|e| e.name == bucket_name),
        "deleted bucket should not be in listing"
    );
}

#[tokio::test]
#[ignore = "needs localstack (docker compose up -d localstack)"]
async fn test_s3_file_operations_and_folders() {
    let bucket_name = format!("test-ops-{}", std::process::id());
    let root_fs = S3Fs::new(test_config(None));
    wait_for_s3(&root_fs).await;

    let bucket_path = Path::new("/").join(&bucket_name);
    root_fs.mkdir(&bucket_path).await.expect("create bucket");

    // Now test with bucket-pinned S3Fs
    let fs = S3Fs::new(test_config(Some(&bucket_name)));

    // 1. Create a folder (directory marker)
    let folder = Path::new("/documents");
    fs.mkdir(folder).await.expect("mkdir folder");

    // 2. Write a file inside folder
    let file_path = Path::new("/documents/hello.txt");
    fs.write_file(file_path, b"Hello from Switchyard S3!")
        .await
        .expect("write file");

    // 3. Stat the file
    let st = fs.stat(file_path).await.expect("stat file");
    assert_eq!(st.name, "hello.txt");
    assert_eq!(st.size, 25);
    assert_eq!(st.kind, EntryKind::File);

    // 4. Read the file
    let content = fs.read_file(file_path, 1024).await.expect("read file");
    assert_eq!(content, b"Hello from Switchyard S3!");

    // 5. List the folder
    let entries = fs.list(folder).await.expect("list folder");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");
    assert_eq!(entries[0].kind, EntryKind::File);

    // 6. Resume / range reading
    let mut reader = fs.open_read_from(file_path, 11).await.expect("open_read_from");
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).await.expect("read tail");
    assert_eq!(tail, b"Switchyard S3!");

    // 7. Rename the file
    let new_path = Path::new("/documents/renamed.txt");
    fs.rename(file_path, new_path).await.expect("rename file");

    assert!(fs.stat(file_path).await.is_err(), "old file should be gone");
    let st_new = fs.stat(new_path).await.expect("stat renamed file");
    assert_eq!(st_new.size, 25);

    // 8. Delete file and cleanup
    fs.delete(new_path).await.expect("delete file");
    fs.delete(folder).await.expect("delete folder");
    root_fs.delete(&bucket_path).await.expect("delete bucket");
}
