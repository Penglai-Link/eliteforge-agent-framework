//! SQLite extension loading adapted from the host's eliteforge_agent_sqlite_extensions module.
//! The caller owns a trusted, private cache directory. No business types,
//! environment variables or source-checkout paths are needed at runtime.
#[path = "assets.rs"]
mod assets;
use rusqlite::{Connection, LoadExtensionGuard};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExtensionMode {
    #[default]
    Disabled,
    Preferred,
    Required,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionStatus {
    Disabled,
    Available,
    Unavailable(String),
}
#[derive(Debug, Clone)]
pub struct ExtensionReport {
    pub fts5: bool,
    pub simple: ExtensionStatus,
    pub vector: ExtensionStatus,
    pub platform: Platform,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub environment: String,
    pub abi: String,
}
impl Platform {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            environment: if cfg!(target_env = "gnu") {
                "gnu"
            } else if cfg!(target_env = "musl") {
                "musl"
            } else if cfg!(target_env = "msvc") {
                "msvc"
            } else if cfg!(target_env = "ohos") {
                "ohos"
            } else {
                ""
            }
            .into(),
            abi: if cfg!(target_abi = "sim") { "sim" } else { "" }.into(),
        }
    }
    pub fn asset_platform(&self) -> Option<String> {
        let arch = match self.arch.as_str() {
            "aarch64" | "arm64" => "aarch64",
            "x86_64" | "amd64" => "x86_64",
            "x86" | "i386" | "i686" => "i686",
            _ => return None,
        };
        let os = match self.os.as_str() {
            "linux" if self.environment == "ohos" => "ohos",
            "linux" if self.environment == "gnu" => "linux",
            "macos" => "macos",
            "windows" => "windows",
            "android" => "android",
            "ios" if self.abi == "sim" || arch == "x86_64" => "iossimulator",
            "ios" => "ios",
            "ohos" => "ohos",
            _ => return None,
        };
        Some(format!("{os}-{arch}"))
    }
}
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("native extensions were not bundled; enable the bundled-native feature")]
    NotBundled,
    #[error("no bundled extension for this operating system, architecture and ABI")]
    UnsupportedPlatform,
    #[error("native extension cache operation failed")]
    Cache,
    #[error("native extension cache contains unexpected data or a symbolic link")]
    Integrity,
    #[error("SQLite extension {operation} failed")]
    Load {
        operation: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    #[error("SQLite connection worker unavailable")]
    Worker,
}
#[derive(Debug, Clone)]
pub struct ExtensionConfig {
    pub cache_dir: PathBuf,
    pub simple: ExtensionMode,
    pub vector: ExtensionMode,
}
impl ExtensionConfig {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            simple: ExtensionMode::Disabled,
            vector: ExtensionMode::Disabled,
        }
    }
    pub fn prepare_connection(
        &self,
        connection: &Connection,
    ) -> Result<ExtensionReport, ExtensionError> {
        let platform = Platform::current();
        let simple = self.prepare(self.simple, || self.load_simple(connection, &platform))?;
        let vector = self.prepare(self.vector, || self.load_vector(connection, &platform))?;
        Ok(ExtensionReport {
            fts5: fts5_available(connection),
            simple,
            vector,
            platform,
        })
    }
    fn prepare(
        &self,
        mode: ExtensionMode,
        load: impl FnOnce() -> Result<(), ExtensionError>,
    ) -> Result<ExtensionStatus, ExtensionError> {
        if mode == ExtensionMode::Disabled {
            return Ok(ExtensionStatus::Disabled);
        }
        match load() {
            Ok(()) => Ok(ExtensionStatus::Available),
            Err(error) if mode == ExtensionMode::Preferred => {
                Ok(ExtensionStatus::Unavailable(error.to_string()))
            }
            Err(error) => Err(error),
        }
    }
    fn load_simple(&self, c: &Connection, platform: &Platform) -> Result<(), ExtensionError> {
        if simple_available(c) {
            return Ok(());
        }
        let filename = if platform.os == "windows" {
            "simple.dll"
        } else if platform.os == "macos" {
            "libsimple.dylib"
        } else {
            "libsimple.so"
        };
        let path = self.native("sqlite-simple/v0.7.1", filename, platform)?;
        load(c, &path)?;
        let prefix = "sqlite-simple/v0.7.1/dict/";
        let dicts: Vec<_> = assets::FILES
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .collect();
        if dicts.is_empty() {
            return Err(ExtensionError::NotBundled);
        }
        let mut digest = Sha256::new();
        for (name, bytes) in &dicts {
            digest.update(name.as_bytes());
            digest.update(bytes);
        }
        let root = self
            .cache_dir
            .join(format!("simple-dict-{:x}", digest.finalize()));
        for (name, bytes) in dicts {
            materialize(&root.join(name.strip_prefix(prefix).unwrap()), bytes)?;
        }
        let absolute = fs::canonicalize(root).map_err(|_| ExtensionError::Cache)?;
        c.query_row(
            "SELECT jieba_dict(?1)",
            [absolute.to_string_lossy()],
            |_| Ok(()),
        )
        .map_err(|source| ExtensionError::Load {
            operation: "dictionary configuration",
            source,
        })?;
        if !simple_available(c) {
            return Err(ExtensionError::Load {
                operation: "capability probe",
                source: rusqlite::Error::InvalidQuery,
            });
        }
        Ok(())
    }
    fn load_vector(&self, c: &Connection, platform: &Platform) -> Result<(), ExtensionError> {
        if vector_available(c) {
            return Ok(());
        }
        let filename = if platform.os == "windows" {
            "vec0.dll"
        } else if matches!(platform.os.as_str(), "macos" | "ios") {
            "vec0.dylib"
        } else {
            "vec0.so"
        };
        let path = self.native("sqlite-vec/v0.1.9", filename, platform)?;
        load(c, &path)?;
        if !vector_available(c) {
            return Err(ExtensionError::Load {
                operation: "capability probe",
                source: rusqlite::Error::InvalidQuery,
            });
        }
        Ok(())
    }
    fn native(
        &self,
        extension: &str,
        filename: &str,
        platform: &Platform,
    ) -> Result<PathBuf, ExtensionError> {
        if !cfg!(feature = "bundled-native") {
            return Err(ExtensionError::NotBundled);
        }
        let platform = platform
            .asset_platform()
            .ok_or(ExtensionError::UnsupportedPlatform)?;
        let key = format!("{extension}/{platform}/{filename}");
        let bytes = assets::FILES
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, bytes)| *bytes)
            .ok_or(ExtensionError::UnsupportedPlatform)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let path = self
            .cache_dir
            .join(extension)
            .join(platform)
            .join(digest)
            .join(filename);
        materialize(&path, bytes)?;
        fs::canonicalize(path).map_err(|_| ExtensionError::Cache)
    }
}
fn load(connection: &Connection, path: &Path) -> Result<(), ExtensionError> {
    // Only this package's embedded, verified native resources reach this boundary.
    // The caller must protect the cache directory from writes by untrusted processes.
    unsafe {
        let _guard =
            LoadExtensionGuard::new(connection).map_err(|source| ExtensionError::Load {
                operation: "enable loading",
                source,
            })?;
        connection
            .load_extension(path, None::<&str>)
            .map_err(|source| ExtensionError::Load {
                operation: "load library",
                source,
            })
    }
}
fn materialize(path: &Path, bytes: &[u8]) -> Result<(), ExtensionError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file()
            || metadata.len() != bytes.len() as u64
            || fs::read(path).map_err(|_| ExtensionError::Cache)? != bytes
        {
            return Err(ExtensionError::Integrity);
        }
        return Ok(());
    }
    let parent = path.parent().ok_or(ExtensionError::Cache)?;
    fs::create_dir_all(parent).map_err(|_| ExtensionError::Cache)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ExtensionError::Cache)?;
    temporary
        .write_all(bytes)
        .map_err(|_| ExtensionError::Cache)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| ExtensionError::Cache)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(failure) if failure.error.kind() == std::io::ErrorKind::AlreadyExists => {
            materialize(path, bytes)
        }
        Err(_) => Err(ExtensionError::Cache),
    }
}
fn tokenizer_available(connection: &Connection, tokenizer: &str) -> bool {
    // Fixed, package-owned identifiers; never interpolate caller data into SQL.
    let sql = if tokenizer == "simple" {
        "CREATE VIRTUAL TABLE temp.agent_sqlite_simple_probe USING fts5(content, tokenize='simple'); DROP TABLE temp.agent_sqlite_simple_probe;"
    } else {
        "CREATE VIRTUAL TABLE temp.agent_sqlite_fts_probe USING fts5(content); DROP TABLE temp.agent_sqlite_fts_probe;"
    };
    connection.execute_batch(sql).is_ok()
}
pub fn fts5_available(c: &Connection) -> bool {
    tokenizer_available(c, "")
}
pub fn simple_available(c: &Connection) -> bool {
    tokenizer_available(c, "simple")
        && c.query_row(
            "SELECT jieba_query('中华人民共和国国歌')",
            [],
            |r| r.get::<_, String>(0),
        )
        .is_ok_and(|s| !s.trim().is_empty())
}
pub fn vector_available(c: &Connection) -> bool {
    c.query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
        .is_ok_and(|s| !s.trim().is_empty())
}

#[async_trait::async_trait]
pub trait SqliteExtensionHost {
    async fn prepare_extensions(
        &self,
        config: ExtensionConfig,
    ) -> Result<ExtensionReport, ExtensionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_mapping_keeps_os_cpu_environment_and_ios_simulator_distinct() {
        let platform = |os: &str, arch: &str, environment: &str, abi: &str| Platform {
            os: os.into(),
            arch: arch.into(),
            environment: environment.into(),
            abi: abi.into(),
        };
        assert_eq!(
            platform("macos", "arm64", "", "")
                .asset_platform()
                .as_deref(),
            Some("macos-aarch64")
        );
        assert_eq!(
            platform("windows", "amd64", "msvc", "")
                .asset_platform()
                .as_deref(),
            Some("windows-x86_64")
        );
        assert_eq!(
            platform("linux", "aarch64", "gnu", "")
                .asset_platform()
                .as_deref(),
            Some("linux-aarch64")
        );
        assert_eq!(
            platform("linux", "aarch64", "musl", "").asset_platform(),
            None
        );
        assert_eq!(
            platform("ios", "aarch64", "", "sim")
                .asset_platform()
                .as_deref(),
            Some("iossimulator-aarch64")
        );
        assert_eq!(
            platform("ios", "aarch64", "", "")
                .asset_platform()
                .as_deref(),
            Some("ios-aarch64")
        );
        assert_eq!(
            platform("linux", "riscv64", "gnu", "").asset_platform(),
            None
        );
    }
    #[test]
    fn disabled_and_preferred_loading_return_explicit_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let connection = Connection::open(dir.path().join("test.db")).unwrap();
        let mut config = ExtensionConfig::new(dir.path().join("native"));
        let report = config.prepare_connection(&connection).unwrap();
        assert!(report.fts5);
        assert_eq!(report.simple, ExtensionStatus::Disabled);
        assert_eq!(report.vector, ExtensionStatus::Disabled);
        assert!(!config.cache_dir.exists());
        if !cfg!(feature = "bundled-native") {
            config.simple = ExtensionMode::Preferred;
            assert!(matches!(
                config.prepare_connection(&connection).unwrap().simple,
                ExtensionStatus::Unavailable(_)
            ));
            config.simple = ExtensionMode::Required;
            assert!(matches!(
                config.prepare_connection(&connection),
                Err(ExtensionError::NotBundled)
            ));
        }
    }
    #[test]
    fn cache_publication_is_atomic_and_checks_content_not_only_file_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || materialize(&path, b"extension"))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), b"extension");
        fs::write(&path, b"corrupted").unwrap();
        assert!(matches!(
            materialize(&path, b"extension"),
            Err(ExtensionError::Integrity)
        ));
    }
    #[cfg(all(
        feature = "bundled-native",
        any(
            all(
                target_os = "macos",
                any(target_arch = "aarch64", target_arch = "x86_64")
            ),
            all(
                target_os = "linux",
                target_env = "gnu",
                any(target_arch = "aarch64", target_arch = "x86_64")
            ),
            all(target_os = "windows", target_arch = "x86_64")
        )
    ))]
    #[test]
    fn real_native_libraries_work_on_independent_connections_without_the_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ExtensionConfig::new(dir.path().join("native"));
        config.simple = ExtensionMode::Required;
        config.vector = ExtensionMode::Required;
        for index in 0..2 {
            let connection = Connection::open(dir.path().join(format!("test-{index}.db"))).unwrap();
            let report = config.prepare_connection(&connection).unwrap();
            assert_eq!(report.simple, ExtensionStatus::Available);
            assert_eq!(report.vector, ExtensionStatus::Available);
            connection.execute_batch("CREATE VIRTUAL TABLE docs USING fts5(text, tokenize='simple'); INSERT INTO docs VALUES ('中华人民共和国国歌');").unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM docs WHERE docs MATCH jieba_query('国歌')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            let length: i64 = connection
                .query_row("SELECT vec_length('[1,2,3]')", [], |r| r.get(0))
                .unwrap();
            assert_eq!(length, 3);
            // Extension loading must be disabled again after preparation.
            let error = connection
                .query_row("SELECT load_extension('missing')", [], |_| Ok(()))
                .unwrap_err();
            assert!(error.to_string().contains("not authorized"), "{error}");
        }
    }
}
