use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

use sha2::{Digest, Sha256};

#[test]
fn current_directory_can_be_bound_as_private_input() {
    bind_private_parent(Path::new(".")).unwrap();
}

#[test]
fn concurrent_identical_blob_publication_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("blob");
    let bytes = b"fictional shared attachment";
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let other_barrier = Arc::clone(&barrier);
        let other_path = path.clone();
        let other_digest = sha256.clone();
        let handle = scope.spawn(move || {
            other_barrier.wait();
            publish_content_addressed_blob(&other_path, &other_digest, bytes)
        });
        barrier.wait();
        publish_content_addressed_blob(&path, &sha256, bytes).unwrap();
        handle.join().unwrap().unwrap();
    });
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!PathBuf::from(format!("{}.staging", path.display())).exists());
}
