use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
};

const ACL_FILE_NAME: &str = "acl.list";

fn acl_path() -> std::path::PathBuf {
    crate::utils::dirs::service_config_dir().join(ACL_FILE_NAME)
}

pub async fn create_acl_file() -> Result<(), anyhow::Error> {
    let path = acl_path();
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    File::create(path).await?;
    Ok(())
}

pub async fn read_acl_file() -> Result<Vec<String>, anyhow::Error> {
    let path = acl_path();
    if !path.exists() {
        return Ok(vec![]);
    }

    let mut file = File::open(path).await?;
    let mut content = String::with_capacity(256);
    file.read_to_string(&mut content).await?;

    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

pub async fn write_acl_file<T: AsRef<str>>(entries: &[T]) -> Result<(), anyhow::Error> {
    let path = acl_path();
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    file.write_all(
        entries
            .iter()
            .map(|x| x.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    )
    .await?;
    Ok(())
}
