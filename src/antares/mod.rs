//! # Antares: Union Filesystem Overlay Manager
//!
//! Antares provides a union filesystem overlay system for managing copy-on-write
//! workspaces on top of a read-only base (Dicfuse). It is designed for monorepo
//! build systems where each build job needs an isolated writable view of the
//! source tree without actually modifying the base files.
//!
//! ## Key Components
//!
//! - [`AntaresPaths`]: Configuration for layer and state directories
//! - [`AntaresConfig`]: Per-mount configuration (job_id, source path, paths, etc.)
//! - [`AntaresManager`]: Manages mount lifecycle (create, unmount, list)
//!
//! ## Layer Stack
//!
//! Antares composes a three-layer union filesystem:
//!
//! ```text
//! ┌─────────────────┐
//! │   upper (rw)    │  ← Job-specific writes
//! ├─────────────────┤
//! │    CL (rw)      │  ← Optional changelist overlay
//! ├─────────────────┤
//! │  Dicfuse (ro)   │  ← Base monorepo tree or mounted sub-path
//! └─────────────────┘
//! ```
//!
//! ## Example
//!
//! ```rust,ignore
//! use scorpiofs::antares::{AntaresManager, AntaresPaths};
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let paths = AntaresPaths::from_global_config();
//!     let manager = AntaresManager::new(paths).await;
//!
//!     // Mount monorepo root at an auto-generated path.
//!     let config = manager.mount_job("build-42", Some("cl-123")).await?;
//!     println!("Mounted at: {}", config.mountpoint.display());
//!
//!     // Or mount a sub-project root directly.
//!     let scoped = manager
//!         .mount_job_for_path("build-43", "/project/foo/bar", None)
//!         .await?;
//!     println!("Scoped mount at: {}", scoped.mountpoint.display());
//!
//!     // Or mount to any custom directory.
//!     let custom_config = manager
//!         .mount_job_at_for_path(
//!             "build-44",
//!             PathBuf::from("/home/user/my-workspace"),
//!             "/project/foo/bar",
//!             None,
//!         )
//!         .await?;
//!
//!     manager.umount_job("build-42").await?;
//!     manager.umount_job("build-43").await?;
//!     manager.umount_job("build-44").await?;
//!     Ok(())
//! }
//! ```

pub mod fuse;

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    dicfuse::{Dicfuse, DicfuseManager},
    util::config,
};

use fuse::AntaresFuse;

const DEFAULT_SOURCE_PATH: &str = "/";
const DICFUSE_READY_TIMEOUT: Duration = Duration::from_secs(15);
const DICFUSE_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn default_source_path() -> String {
    DEFAULT_SOURCE_PATH.to_string()
}

fn normalize_source_path(source_path: &str) -> String {
    let trimmed = source_path.trim();
    if trimmed.is_empty() || trimmed == DEFAULT_SOURCE_PATH {
        DEFAULT_SOURCE_PATH.to_string()
    } else {
        format!("/{}", trimmed.trim_matches('/'))
    }
}

fn normalize_ready_path(ready_path: Option<&str>) -> Option<String> {
    let ready_path = ready_path?.trim();
    if ready_path.is_empty() || ready_path == DEFAULT_SOURCE_PATH {
        None
    } else {
        Some(format!("/{}", ready_path.trim_matches('/')))
    }
}

async fn probe_mountpoint(mountpoint: &Path, ready_path: Option<&str>) -> io::Result<()> {
    let metadata = tokio::fs::metadata(mountpoint).await?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", mountpoint.display()),
        ));
    }

    let mut entries = tokio::fs::read_dir(mountpoint).await?;
    if let Some(entry) = entries.next_entry().await? {
        let _ = entry.file_type().await?;
    }

    let Some(ready_path) = normalize_ready_path(ready_path) else {
        return Ok(());
    };

    let ready_fs_path = mountpoint.join(ready_path.trim_start_matches('/'));
    let ready_metadata = tokio::fs::metadata(&ready_fs_path).await?;
    if ready_metadata.is_dir() {
        let mut ready_entries = tokio::fs::read_dir(&ready_fs_path).await?;
        if let Some(entry) = ready_entries.next_entry().await? {
            let _ = entry.file_type().await?;
        }
    } else {
        let _ = tokio::fs::File::open(&ready_fs_path).await?;
    }

    Ok(())
}

/// Global paths used by Antares to place layers and state.
#[derive(Debug, Clone)]
pub struct AntaresPaths {
    /// Root directory to place per-job upper layers.
    pub upper_root: PathBuf,
    /// Root directory to place per-job CL layers when requested.
    pub cl_root: PathBuf,
    /// Base directory for mountpoints returned to callers.
    pub mount_root: PathBuf,
    /// Path to persist mount state as TOML.
    pub state_file: PathBuf,
}

impl AntaresPaths {
    pub fn new(
        upper_root: PathBuf,
        cl_root: PathBuf,
        mount_root: PathBuf,
        state_file: PathBuf,
    ) -> Self {
        Self {
            upper_root,
            cl_root,
            mount_root,
            state_file,
        }
    }

    /// Build paths using global config defaults.
    pub fn from_global_config() -> Self {
        Self {
            upper_root: PathBuf::from(config::antares_upper_root()),
            cl_root: PathBuf::from(config::antares_cl_root()),
            mount_root: PathBuf::from(config::antares_mount_root()),
            state_file: PathBuf::from(config::antares_state_file()),
        }
    }
}

/// Persisted config for a mounted Antares job instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntaresConfig {
    pub job_id: String,
    #[serde(default = "default_source_path")]
    pub source_path: String,
    pub mountpoint: PathBuf,
    pub upper_id: String,
    pub upper_dir: PathBuf,
    pub cl_dir: Option<PathBuf>,
    pub cl_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AntaresState {
    mounts: Vec<AntaresConfig>,
}

/// Manager responsible for creating and tracking Antares overlay instances.
pub struct AntaresManager {
    dic: Arc<Dicfuse>,
    paths: AntaresPaths,
    instances: Arc<Mutex<HashMap<String, AntaresConfig>>>,
    fuse_handles: Arc<Mutex<HashMap<String, AntaresFuse>>>,
}

impl AntaresManager {
    fn with_dicfuse(paths: AntaresPaths, dic: Arc<Dicfuse>) -> Self {
        let instances = Self::load_state(&paths.state_file).unwrap_or_default();
        Self {
            dic,
            paths,
            instances: Arc::new(Mutex::new(instances)),
            fuse_handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build an independent Antares manager with its own Dicfuse instance.
    pub async fn new(paths: AntaresPaths) -> Self {
        let dic = DicfuseManager::global().await;
        Self::with_dicfuse(paths, dic)
    }

    /// Build an Antares manager using a caller-provided Dicfuse instance.
    pub async fn new_with_dicfuse(paths: AntaresPaths, dic: Arc<Dicfuse>) -> Self {
        Self::with_dicfuse(paths, dic)
    }

    /// Build an Antares manager backed by an isolated on-disk Dicfuse store.
    pub async fn new_with_store_path(paths: AntaresPaths, store_path: &str) -> Self {
        let dic = Arc::new(Dicfuse::new_with_store_path(store_path).await);
        Self::with_dicfuse(paths, dic)
    }

    /// Mount the monorepo root at an auto-generated mountpoint.
    pub async fn mount_job(
        &self,
        job_id: &str,
        cl_name: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        self.mount_job_for_path(job_id, DEFAULT_SOURCE_PATH, cl_name)
            .await
    }

    /// Mount a specific repository sub-path at an auto-generated mountpoint.
    pub async fn mount_job_for_path(
        &self,
        job_id: &str,
        source_path: &str,
        cl_name: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        self.mount_job_for_path_with_ready_path(job_id, source_path, cl_name, None)
            .await
    }

    /// Mount a specific repository sub-path and require an additional ready path.
    pub async fn mount_job_for_path_with_ready_path(
        &self,
        job_id: &str,
        source_path: &str,
        cl_name: Option<&str>,
        ready_path: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        let mountpoint = self.paths.mount_root.join(job_id);
        self.mount_job_at_for_path_with_ready_path(
            job_id,
            mountpoint,
            source_path,
            cl_name,
            ready_path,
        )
        .await
    }

    /// Mount the monorepo root at a caller-provided mountpoint.
    pub async fn mount_job_at(
        &self,
        job_id: &str,
        mountpoint: impl Into<PathBuf>,
        cl_name: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        self.mount_job_at_for_path_with_ready_path(
            job_id,
            mountpoint,
            DEFAULT_SOURCE_PATH,
            cl_name,
            None,
        )
        .await
    }

    /// Mount a specific repository sub-path at a caller-provided mountpoint.
    pub async fn mount_job_at_for_path(
        &self,
        job_id: &str,
        mountpoint: impl Into<PathBuf>,
        source_path: &str,
        cl_name: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        self.mount_job_at_for_path_with_ready_path(job_id, mountpoint, source_path, cl_name, None)
            .await
    }

    /// Mount a specific repository sub-path and require an additional ready path.
    pub async fn mount_job_at_for_path_with_ready_path(
        &self,
        job_id: &str,
        mountpoint: impl Into<PathBuf>,
        source_path: &str,
        cl_name: Option<&str>,
        ready_path: Option<&str>,
    ) -> std::io::Result<AntaresConfig> {
        let mountpoint = mountpoint.into();
        let source_path = normalize_source_path(source_path);
        let ready_path = normalize_ready_path(ready_path);
        let start = std::time::Instant::now();
        tracing::info!(
            "antares: mount_job_at start job_id={} source_path={} ready_path={:?} mountpoint={} cl={:?}",
            job_id,
            source_path,
            ready_path,
            mountpoint.display(),
            cl_name
        );

        // Prepare per-job paths.
        let upper_id = Uuid::new_v4().to_string();
        let upper_dir = self.paths.upper_root.join(&upper_id);
        let (cl_id, cl_dir) = match cl_name {
            Some(_) => {
                let id = Uuid::new_v4().to_string();
                (Some(id.clone()), Some(self.paths.cl_root.join(id)))
            }
            None => (None, None),
        };

        std::fs::create_dir_all(&upper_dir)?;
        if let Some(cl) = &cl_dir {
            std::fs::create_dir_all(cl)?;
        }
        std::fs::create_dir_all(&mountpoint)?;

        let instance = AntaresConfig {
            job_id: job_id.to_string(),
            source_path: source_path.clone(),
            mountpoint,
            upper_id,
            upper_dir,
            cl_dir,
            cl_id,
        };

        self.instances
            .lock()
            .await
            .insert(job_id.to_string(), instance.clone());
        self.persist_state().await?;

        let dic = DicfuseManager::for_base_path(&source_path).await;
        let store_ready_path = ready_path.as_deref().unwrap_or(DEFAULT_SOURCE_PATH);
        dic.store
            .wait_for_path_ready(
                store_ready_path,
                DICFUSE_READY_TIMEOUT,
                DICFUSE_READY_POLL_INTERVAL,
            )
            .await?;

        let mut fuse = AntaresFuse::new(
            instance.mountpoint.clone(),
            dic,
            instance.upper_dir.clone(),
            instance.cl_dir.clone(),
        )
        .await?;

        if let Err(err) = fuse.mount().await {
            self.rollback_failed_mount(job_id, &instance, Some(&mut fuse))
                .await;
            return Err(err);
        }

        if let Err(err) = probe_mountpoint(&instance.mountpoint, ready_path.as_deref()).await {
            self.rollback_failed_mount(job_id, &instance, Some(&mut fuse))
                .await;
            return Err(io::Error::other(format!(
                "mount probe failed on {}: {}",
                instance.mountpoint.display(),
                err
            )));
        }

        self.fuse_handles
            .lock()
            .await
            .insert(job_id.to_string(), fuse);

        tracing::info!(
            "antares: mount_job done job_id={} source_path={} mountpoint={} elapsed={:.2}s",
            job_id,
            source_path,
            instance.mountpoint.display(),
            start.elapsed().as_secs_f64()
        );
        Ok(instance)
    }

    async fn rollback_failed_mount(
        &self,
        job_id: &str,
        instance: &AntaresConfig,
        fuse: Option<&mut AntaresFuse>,
    ) {
        if let Some(fuse) = fuse {
            fuse.unmount().await.ok();
        }
        let _ = std::fs::remove_dir_all(&instance.mountpoint);
        let _ = std::fs::remove_dir_all(&instance.upper_dir);
        if let Some(cl) = &instance.cl_dir {
            let _ = std::fs::remove_dir_all(cl);
        }
        self.fuse_handles.lock().await.remove(job_id);
        self.instances.lock().await.remove(job_id);
        self.persist_state().await.ok();
    }

    /// Unmount the FUSE filesystem and remove bookkeeping for a job.
    pub async fn umount_job(&self, job_id: &str) -> std::io::Result<Option<AntaresConfig>> {
        use tracing::{info, warn};

        let instances = self.instances.lock().await;
        let config = match instances.get(job_id) {
            Some(cfg) => cfg.clone(),
            None => return Ok(None),
        };
        drop(instances);

        let mount_path = &config.mountpoint;
        info!("Attempting to unmount FUSE mount at {:?}", mount_path);

        let mut unmounted = false;
        if let Some(mut fuse) = self.fuse_handles.lock().await.remove(job_id) {
            match fuse.unmount().await {
                Ok(_) => {
                    info!("Successfully unmounted {:?} via AntaresFuse", mount_path);
                    unmounted = true;
                }
                Err(err) => {
                    warn!(
                        "AntaresFuse::unmount failed for {:?}: {}; falling back to fusermount -u",
                        mount_path, err
                    );
                }
            }
        }

        if !unmounted {
            let output = tokio::process::Command::new("fusermount")
                .arg("-u")
                .arg(mount_path)
                .output()
                .await?;

            if !output.status.success() {
                let error_msg = String::from_utf8_lossy(&output.stderr);
                if error_msg.contains("not mounted") || error_msg.contains("Invalid argument") {
                    warn!(
                        "Filesystem at {:?} is not mounted, removing bookkeeping only: {}",
                        mount_path, error_msg
                    );
                } else {
                    warn!(
                        "fusermount -u failed with status {} for {:?}: {}",
                        output.status, mount_path, error_msg
                    );
                }
            } else {
                info!("Successfully unmounted {:?} via fusermount -u", mount_path);
            }
        }

        let mut instances = self.instances.lock().await;
        let removed = instances.remove(job_id);
        drop(instances);
        self.persist_state().await?;

        Ok(removed)
    }

    /// List all tracked instances.
    pub async fn list(&self) -> Vec<AntaresConfig> {
        self.instances.lock().await.values().cloned().collect()
    }

    /// Access the underlying root-view Dicfuse instance.
    pub fn dicfuse(&self) -> Arc<Dicfuse> {
        self.dic.clone()
    }

    /// Check whether the FUSE session for a given job is still alive.
    pub async fn is_job_alive(&self, job_id: &str) -> bool {
        self.fuse_handles
            .lock()
            .await
            .get(job_id)
            .map_or(false, |f| f.is_session_alive())
    }

    fn load_state(path: &Path) -> std::io::Result<HashMap<String, AntaresConfig>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let content = fs::read_to_string(path)?;
        let state: AntaresState = toml::from_str(&content).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("parse state: {e}"))
        })?;
        let mut map = HashMap::new();
        for mount in state.mounts {
            map.insert(mount.job_id.clone(), mount);
        }
        Ok(map)
    }

    async fn persist_state(&self) -> std::io::Result<()> {
        let mounts: Vec<AntaresConfig> = self.instances.lock().await.values().cloned().collect();
        let state = AntaresState { mounts };
        let data = toml::to_string_pretty(&state)
            .map_err(|e| std::io::Error::other(format!("encode state: {e}")))?;
        if let Some(parent) = self.paths.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&self.paths.state_file)?;
        file.write_all(data.as_bytes())?;
        Ok(())
    }
}
