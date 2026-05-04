use anyhow::{Context, bail};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub fn write_token(path: &Path, token: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("open token file {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}

pub fn read_token(path: &Path) -> anyhow::Result<String> {
    validate_token_path(path)?;
    let token = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(token.trim().to_string())
}

pub fn remove_token(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn validate_token_path(path: &Path) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("token path {} has no parent", path.display()))?;
    let dir_mode = fs::metadata(dir)?.permissions().mode() & 0o777;
    if dir_mode != 0o700 {
        bail!("token directory permissions must be 0700");
    }
    let file_mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if file_mode != 0o600 {
        bail!("token file permissions must be 0600");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_token_with_0600_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token-dir/token");
        write_token(&path, "mcp_test").expect("write token");
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_token(&path).expect("read token"), "mcp_test");
    }

    #[test]
    fn rejects_broad_token_file_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_dir = dir.path().join("token-dir");
        fs::create_dir_all(&token_dir).expect("create token dir");
        fs::set_permissions(&token_dir, fs::Permissions::from_mode(0o700)).expect("chmod dir");
        let path = token_dir.join("token");
        fs::write(&path, "mcp_test").expect("write token");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod file");
        let error = validate_token_path(&path).expect_err("broad mode rejected");
        assert!(error.to_string().contains("0600"));
    }
}
