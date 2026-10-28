use crate::model::Run;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    fn directory(&self, name: &str) -> Result<PathBuf> {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(dir)
    }
    pub fn blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = format!("{:x}", Sha256::digest(bytes));
        let path = self.directory("blobs")?.join(&hash);
        if !path.exists() {
            atomic_write(&path, bytes)?;
        }
        Ok(hash)
    }
    pub fn save(&self, run: &Run) -> Result<()> {
        atomic_write(
            &self.directory("runs")?.join(format!("{}.json", run.run_id)),
            &serde_json::to_vec(run)?,
        )
    }
    pub fn load(&self, id: &str) -> Result<Run> {
        ensure!(uuid::Uuid::parse_str(id).is_ok(), "run ID must be a UUID");
        let data = std::fs::read(self.root.join("runs").join(format!("{id}.json")))
            .with_context(|| format!("cannot read saved run {id}"))?;
        Ok(serde_json::from_slice(&data)?)
    }
    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "artifact must be a SHA-256 hash"
        );
        Ok(std::fs::read(self.root.join("blobs").join(hash))?)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_path_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("store"));
        let run = Run::new("test");
        store.save(&run).unwrap();
        assert_eq!(store.load(&run.run_id).unwrap().run_id, run.run_id);
        let hash = store.blob(b"original bytes").unwrap();
        assert_eq!(store.read_blob(&hash).unwrap(), b"original bytes");
        assert!(store.load("../../etc/passwd").is_err());
        assert!(store.read_blob("../../etc/passwd").is_err());
    }
}
