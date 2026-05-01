use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: u32,
    pub provider: String,
    pub subject: String,
    pub email: String,
    pub name: String,
}

pub struct UserStore {
    dir: PathBuf,
}

impl UserStore {
    pub async fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).await.unwrap_or_else(|e| {
            panic!("create users dir {}: {e}", dir.display())
        });
        Self { dir }
    }

    /// Hex-encoded SHA-256 of the email — used as the file name.
    pub fn email_key(email: &str) -> String {
        hex::encode(Sha256::digest(email.as_bytes()))
    }

    /// First 4 bytes of SHA-256(email) as a little-endian u32.
    pub fn email_id(email: &str) -> u32 {
        let hash = Sha256::digest(email.as_bytes());
        u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]])
    }

    pub async fn get(&self, email: &str) -> io::Result<Option<User>> {
        let path = self.dir.join(format!("{}.json", Self::email_key(email)));
        match fs::read(&path).await {
            Ok(bytes) => {
                let user = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(user))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Atomic write: tmp file → fsync → rename.
    pub async fn upsert(&self, user: &User) -> io::Result<()> {
        let path = self.dir.join(format!("{}.json", Self::email_key(&user.email)));
        let tmp = self.dir.join(format!("{}.tmp", Self::email_key(&user.email)));
        let bytes = serde_json::to_vec_pretty(user)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, &bytes).await?;
        fs::rename(&tmp, &path).await
    }
}
