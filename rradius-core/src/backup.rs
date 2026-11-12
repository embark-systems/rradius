//! Backups of the cluster database, with the `backup` feature: what `[cluster.backup]` holds once
//! an extension has read it, and how the hiqlite node is configured from it. hiqlite makes the
//! copies; the extension decides when, so that a cluster makes one backup at a time and not one
//! per node.

use std::path::PathBuf;
use std::sync::Arc;

/// `[cluster.backup]`, as the extension that reads it made it, kept in `ServerConfig` with the
/// other settings of extensions.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterBackup {
    /// Minutes between backups.
    pub interval_minutes: u64,
    /// Days the copies on this node, and in `directory`, are kept.
    pub keep_days: u64,
    /// A directory each node copies its backups into, made absolute.
    pub directory: Option<PathBuf>,
    pub s3: Option<S3Backup>,
}

/// `[cluster.backup.s3]`, with its files read.
#[derive(Clone, PartialEq)]
pub struct S3Backup {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub path_style: bool,
    /// Days the objects in the bucket are kept.
    pub keep_days: u64,
    /// The key the copies are encrypted with, as `<id>/<32 bytes in base64>`.
    pub encryption_key: String,
}

impl std::fmt::Debug for S3Backup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Backup")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("path_style", &self.path_style)
            .field("keep_days", &self.keep_days)
            .finish_non_exhaustive()
    }
}

/// A schedule with no next time: hiqlite's own backup task, which every node runs, then never
/// makes a backup.
const NEVER: &str = "0 0 0 30 2 * *";

/// The names of the variables of the environment that hiqlite or its encryption read, sorted.
fn hiqlite_variables() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()).filter(|name| name.starts_with("HQL_") || name.starts_with("ENC_")).collect();
    names.sort();
    names
}

/// Reads an encryption key, `<id>/<32 bytes in base64>`.
pub fn encryption_key(text: &str) -> Result<cryptr::EncKeys, String> {
    let text = text.trim();
    let (id, _) = text.split_once('/').ok_or("an encryption key is written as <id>/<32 bytes in base64>")?;
    let keys = cryptr::EncKeys::try_parse(id.to_string(), vec![text.to_string()]).map_err(|e| format!("encryption key: {e}"))?;
    if keys.enc_keys.is_empty() {
        return Err("an encryption key is written as <id>/<32 bytes in base64>".to_string());
    }
    Ok(keys)
}

/// Sets up `config`'s backups: none of hiqlite's own schedule in any case, and with `backup`, how
/// long copies are kept, the bucket and the key.
pub(crate) async fn configure(config: &mut hiqlite::NodeConfig, backup: Option<&ClusterBackup>) -> Result<(), String> {
    // hiqlite's schedule would have every node ask for a backup; another variable set for it
    // would bring that back.
    if std::env::var_os("HQL_BACKUP_CRON").is_some() {
        return Err("HQL_BACKUP_CRON is set: backups are scheduled by [cluster.backup], unset it".to_string());
    }
    let keys = match backup.and_then(|backup| backup.s3.as_ref()) {
        Some(s3) => encryption_key(&s3.encryption_key)?,
        // hiqlite refuses to start without a key, also with nothing to encrypt.
        None => {
            let mut keys = cryptr::EncKeys::default();
            keys.append_new_random().map_err(|e| format!("cannot make an encryption key: {e}"))?;
            keys
        }
    };
    // A `BackupConfig` cannot be made other than through hiqlite's own configuration file.
    let mut table = toml::Table::new();
    table.insert("backup_cron".into(), NEVER.into());
    table.insert("secret_raft".into(), config.secret_raft.clone().into());
    table.insert("secret_api".into(), config.secret_api.clone().into());
    // hiqlite reads a `.env` file into the environment here, where its own variables would
    // bring back its schedule or restore a backup at the start: refused.
    let before = hiqlite_variables();
    let scheduled = hiqlite::NodeConfig::from_toml_table(table, "hiqlite", None, Some(keys)).await.map_err(|e| format!("backup settings: {e}"))?;
    let added: Vec<String> = hiqlite_variables().into_iter().filter(|name| !before.contains(name)).collect();
    if !added.is_empty() {
        return Err(format!("a .env file set {}: remove it from the .env file, or set it in the environment if it is meant", added.join(", ")));
    }
    config.backup_config = scheduled.backup_config;
    config.enc_keys = scheduled.enc_keys;
    if let Some(backup) = backup {
        config.backup_keep_for_local = std::time::Duration::from_secs(backup.keep_days.saturating_mul(24 * 3600));
        if let Some(s3) = &backup.s3 {
            let bucket = hiqlite::s3::S3Config::new(&s3.endpoint, s3.bucket.clone(), s3.region.clone(), s3.access_key.clone(), s3.secret_key.clone(), s3.path_style).await.map_err(|e| format!("[cluster.backup.s3]: {e}"))?;
            config.s3_config = Some(bucket);
        }
    }
    Ok(())
}

/// The bucket of `[cluster.backup.s3]`, for listing and deleting its objects.
pub async fn bucket(s3: &S3Backup) -> Result<Arc<hiqlite::s3::S3Config>, String> {
    hiqlite::s3::S3Config::new(&s3.endpoint, s3.bucket.clone(), s3.region.clone(), s3.access_key.clone(), s3.secret_key.clone(), s3.path_style).await.map_err(|e| format!("[cluster.backup.s3]: {e}"))
}
