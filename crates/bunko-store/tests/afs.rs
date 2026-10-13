#![cfg(feature = "tokio")]

use bunko_store::afs;

#[tokio::test]
async fn afs_passthrough() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("a/b");
    afs::create_dir_all(&dir).await.unwrap();
    let p = dir.join("f");
    let mut f = afs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    f.write_all(b"hello").await.unwrap();
    f.sync_all().await.unwrap();
    drop(f);
    assert_eq!(afs::metadata(&p).await.unwrap().len(), 5);
    let q = dir.join("g");
    afs::rename(&p, &q).await.unwrap();
    let file = afs::File::open(&q).await.unwrap().into_std().unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 5);
    afs::remove_file(&q).await.unwrap();
    assert!(afs::metadata(&q).await.is_err());
}
