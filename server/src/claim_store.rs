use crate::user_store::UserStore;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub user_id: u32,
    pub cx: i32,
    pub cy: i32,
}

pub struct ClaimStore {
    dir: PathBuf,
}

impl ClaimStore {
    pub async fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).await.unwrap_or_else(|e| {
            panic!("create claims dir {}: {e}", dir.display())
        });
        Self { dir }
    }

    pub async fn get(&self, email: &str) -> io::Result<Option<Claim>> {
        let path = self.dir.join(format!("{}.json", UserStore::email_key(email)));
        match fs::read(&path).await {
            Ok(bytes) => {
                let claim = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(claim))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn put(&self, email: &str, cx: i32, cy: i32, user_id: u32) -> io::Result<()> {
        let key = UserStore::email_key(email);
        let path = self.dir.join(format!("{key}.json"));
        let tmp = self.dir.join(format!("{key}.tmp"));
        let claim = Claim { user_id, cx, cy };
        let bytes = serde_json::to_vec_pretty(&claim)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, &bytes).await?;
        fs::rename(&tmp, &path).await
    }

    pub async fn delete(&self, email: &str) -> io::Result<()> {
        let path = self.dir.join(format!("{}.json", UserStore::email_key(email)));
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Load all claims from disk on startup by scanning the directory.
    pub async fn all(&self) -> io::Result<Vec<Claim>> {
        let mut claims = Vec::new();
        let mut rd = fs::read_dir(&self.dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
            match fs::read(&path).await {
                Ok(bytes) => match serde_json::from_slice::<Claim>(&bytes) {
                    Ok(c) => claims.push(c),
                    Err(e) => tracing::warn!(path = %path.display(), err = %e, "skip malformed claim"),
                },
                Err(e) => tracing::warn!(path = %path.display(), err = %e, "skip unreadable claim"),
            }
        }
        Ok(claims)
    }
}
