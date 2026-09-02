use camino::Utf8PathBuf;
use nyanpasu_core_manager::{RuntimeCommitDurability, RuntimeConfigStore};

#[tokio::test]
async fn staged_files_are_removed_when_abandoned() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let store = RuntimeConfigStore::new(root).await.unwrap();
    let staged = store.stage(7, b"port: 7890\n").await.unwrap();
    let path = staged.path().to_owned();
    assert!(path.exists());
    drop(staged);
    assert!(!path.exists());
}

#[tokio::test]
async fn backup_restore_and_cleanup_form_one_epoch_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let store = RuntimeConfigStore::new(root).await.unwrap();

    let staged = store.stage(3, b"allow-lan: false\n").await.unwrap();
    let runtime = store.commit_new(staged, 3).await.unwrap();
    assert_eq!(tokio::fs::read_to_string(&runtime).await.unwrap(), "allow-lan: false\n");

    let backup = store.backup(3, 2).await.unwrap();
    assert!(backup.path().exists());
    let commit = store.replace(3, b"allow-lan: true\n").await.unwrap();
    assert!(matches!(commit.durability(), RuntimeCommitDurability::Durable));
    assert_eq!(tokio::fs::read_to_string(commit.path()).await.unwrap(), "allow-lan: true\n");

    store.restore(&backup).await.unwrap();
    assert_eq!(tokio::fs::read_to_string(&runtime).await.unwrap(), "allow-lan: false\n");
    store.remove_backup(backup).await.unwrap();
    assert_eq!(store.artifact_epochs().await.unwrap(), vec![3]);

    store.cleanup_epoch(3).await.unwrap();
    assert!(!runtime.exists());
    assert!(store.artifact_epochs().await.unwrap().is_empty());
}
