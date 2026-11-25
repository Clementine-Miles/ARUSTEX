use std::{
    env, fmt, fs,
    io::{self, Read},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use arustex_store::Store;
use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    database: Database,
    local: Local,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Database {
    dsn: String,
    max_connections: u32,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            dsn: String::new(),
            max_connections: 4,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Local {
    socket: Option<PathBuf>,
}

pub struct Config {
    pub socket: PathBuf,
    pub max_connections: u32,
    database_dsn: String,
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let from_env = env::var_os("ARTEX_CONFIG").filter(|value| !value.is_empty());
        let path = explicit.or(from_env.as_deref().map(Path::new));
        let file = read_config(path)?;
        let dsn_override = match env::var("ARTEX_PG_DSN") {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError("ARTEX_PG_DSN is not UTF-8"));
            }
        };
        Self::resolve(
            file,
            dsn_override,
            env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        )
    }

    fn resolve(
        file: FileConfig,
        dsn_override: Option<String>,
        runtime_dir: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let FileConfig { database, local } = file;
        if !(1..=16).contains(&database.max_connections) {
            return Err(ConfigError(
                "database.max_connections must be between 1 and 16",
            ));
        }
        let database_dsn = dsn_override
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(database.dsn);
        let socket = local
            .socket
            .or_else(|| runtime_dir.map(|dir| dir.join("arustex-inspect.sock")))
            .ok_or(ConfigError("set local.socket or XDG_RUNTIME_DIR"))?;
        if !socket.is_absolute()
            || socket.file_name().is_none()
            || socket.as_os_str().len() > 107
            || socket.as_os_str().as_encoded_bytes().contains(&0)
            || socket.components().any(|part| part == Component::ParentDir)
        {
            return Err(ConfigError(
                "local.socket must be an absolute Linux socket path of at most 107 bytes",
            ));
        }
        let config = Self {
            socket,
            max_connections: database.max_connections,
            database_dsn,
        };
        if !config.database_dsn.trim().is_empty() {
            config.database_dsn()?;
        }
        Ok(config)
    }

    pub fn database_dsn(&self) -> Result<&str, ConfigError> {
        let dsn = self.database_dsn.trim();
        if dsn.is_empty() {
            return Err(ConfigError(
                "set database.dsn or ARTEX_PG_DSN for the daemon",
            ));
        }
        Store::validate_dsn(dsn).map_err(|_| {
            ConfigError("use a PostgreSQL URI with an explicit host and sslmode=verify-full for non-loopback connections")
        })?;
        Ok(dsn)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ConfigError(&'static str);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ConfigError {}

fn read_config(explicit: Option<&Path>) -> Result<FileConfig, ConfigError> {
    let path = explicit.unwrap_or(Path::new("config.rust.toml"));
    let fd = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)
    {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound && explicit.is_none() => {
            return Ok(FileConfig::default());
        }
        Err(_) => return Err(ConfigError("cannot open TOML configuration file")),
    };
    let file = fs::File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|_| ConfigError("cannot inspect TOML configuration file"))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError(
            "configuration must be a regular TOML file of at most 64 KiB",
        ));
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| ConfigError("cannot read UTF-8 TOML configuration"))?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError("configuration exceeds 64 KiB"));
    }
    let config: FileConfig = toml::from_str(&text)
        .map_err(|_| ConfigError("invalid TOML or unsupported configuration key"))?;
    if !config.database.dsn.trim().is_empty()
        && (metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw())
    {
        return Err(ConfigError(
            "configuration files containing a database DSN must be owner-only",
        ));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(text: &str, env_dsn: Option<&str>) -> Result<Config, ConfigError> {
        Config::resolve(
            toml::from_str(text).unwrap(),
            env_dsn.map(str::to_owned),
            Some(PathBuf::from("/run/user/1000")),
        )
    }

    #[test]
    fn environment_takes_precedence_and_tui_can_omit_database() {
        let db = resolve(
            "[database]\ndsn='postgres://file@127.0.0.1/db'",
            Some("postgres://env@127.0.0.1/db"),
        )
        .unwrap();
        assert_eq!(db.database_dsn(), Ok("postgres://env@127.0.0.1/db"));
        let local = resolve("", None).unwrap();
        assert_eq!(
            local.socket,
            PathBuf::from("/run/user/1000/arustex-inspect.sock")
        );
        assert!(local.database_dsn().is_err());
    }

    #[test]
    fn dsn_validation_has_safe_errors_and_unambiguous_tls() {
        for dsn in [
            "postgres://user:secret@db.invalid/app?sslmode=require",
            "postgres://user:secret@db.invalid/app?sslmode=verify-full&sslmode=disable",
            "postgres://user:secret@127.0.0.1/app?host=db.invalid",
        ] {
            assert!(
                matches!(resolve("", Some(dsn)), Err(error) if !error.to_string().contains("secret"))
            );
        }
        assert!(resolve("", Some("postgres://db.invalid/app?sslmode=verify-full")).is_ok());
    }

    #[test]
    fn bounds_and_unknown_keys_are_rejected() {
        assert!(resolve("[database]\nmax_connections=0", None).is_err());
        assert!(resolve("[local]\nsocket='relative.sock'", None).is_err());
        assert!(toml::from_str::<FileConfig>("[local]\nunrecognized=true").is_err());
    }
}
