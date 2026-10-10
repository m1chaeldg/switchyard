//! Shared UI-side state types and helpers.

use std::sync::atomic::{AtomicU64, Ordering};

use switchyard_core::store::{DbConnection, FileProtocol, Host, Profile, ProfileId};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A process-unique id for requests, sessions, queries and tabs.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Lifecycle of a database session as the UI sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionState {
    /// No connection chosen.
    None,
    /// Connecting.
    Connecting,
    /// Open.
    Open {
        /// Server version string.
        version: String,
    },
    /// Failed to open.
    Failed(String),
}

/// Read-only view over the saved profiles.
#[derive(Clone, Debug, Default)]
pub struct Profiles {
    /// All profiles in sidebar order.
    pub all: Vec<Profile>,
}

impl Profiles {
    /// A database connection by id.
    pub fn db(&self, id: &ProfileId) -> Option<&DbConnection> {
        self.all.iter().find_map(|p| match p {
            Profile::Db(d) if &d.id == id => Some(d),
            _ => None,
        })
    }

    /// A Host by id.
    pub fn host(&self, id: &ProfileId) -> Option<&Host> {
        self.all.iter().find_map(|p| match p {
            Profile::Host(h) if &h.id == id => Some(h),
            _ => None,
        })
    }

    /// Every Host.
    pub fn hosts(&self) -> impl Iterator<Item = &Host> {
        self.all.iter().filter_map(|p| match p {
            Profile::Host(h) => Some(h),
            _ => None,
        })
    }

    /// Every database connection.
    pub fn dbs(&self) -> impl Iterator<Item = &DbConnection> {
        self.all.iter().filter_map(|p| match p {
            Profile::Db(d) => Some(d),
            _ => None,
        })
    }

    /// Whether nothing is saved yet.
    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// Children of a Host: (badge, label, sub, profile) for terminals, files and databases.
    pub fn host_children(&self, host: &ProfileId) -> Vec<&Profile> {
        self.all
            .iter()
            .filter(|p| match p {
                Profile::Db(d) => d.via_host.as_ref() == Some(host),
                Profile::File(f) => {
                    matches!(&f.protocol, FileProtocol::Sftp { host_id } if host_id == host)
                }
                Profile::Terminal(t) => t.host_id.as_ref() == Some(host),
                Profile::Host(_) => false,
            })
            .collect()
    }

    /// Profiles not attached to a Host (direct databases, FTP, S3, local terminals).
    pub fn direct(&self) -> Vec<&Profile> {
        self.all
            .iter()
            .filter(|p| match p {
                Profile::Db(d) => d.via_host.is_none(),
                Profile::File(f) => matches!(f.protocol, FileProtocol::Ftp { .. } | FileProtocol::S3 { .. }),
                Profile::Terminal(t) => t.host_id.is_none(),
                Profile::Host(_) => false,
            })
            .collect()
    }
}

/// Monogram for a profile.
pub fn badge_of(p: &Profile) -> &'static str {
    match p {
        Profile::Host(_) => "SSH",
        Profile::Db(d) => d.engine.badge(),
        Profile::File(f) => match f.protocol {
            FileProtocol::Sftp { .. } => "SFTP",
            FileProtocol::Ftp { .. } => "FTP",
            FileProtocol::S3 { .. } => "S3",
        },
        Profile::Terminal(_) => "SH",
    }
}

/// A short, host-free description for lists ("PostgreSQL · direct").
pub fn describe(p: &Profile, profiles: &Profiles) -> String {
    match p {
        Profile::Host(h) => format!("{}@{}", h.user, h.address),
        Profile::Db(d) => match &d.via_host {
            Some(h) => format!(
                "via {}",
                profiles.host(h).map(|h| h.name.as_str()).unwrap_or("host")
            ),
            None if d.engine.is_local_file() => std::path::Path::new(d.database.trim())
                .file_name()
                .map_or_else(|| d.database.clone(), |f| f.to_string_lossy().into_owned()),
            None => format!("{}:{}", d.server, d.port),
        },
        Profile::File(f) => match &f.protocol {
            FileProtocol::Sftp { .. } => "SFTP".into(),
            FileProtocol::Ftp { server, tls, .. } => format!(
                "{server} · {}",
                match tls {
                    switchyard_core::store::FtpTls::None => "ftp",
                    switchyard_core::store::FtpTls::Explicit => "ftps · explicit",
                    switchyard_core::store::FtpTls::Implicit => "ftps · implicit",
                }
            ),
            FileProtocol::S3 { region, bucket, .. } => match bucket {
                Some(b) => format!("{b} · {region}"),
                None => format!("all buckets · {region}"),
            },
        },
        Profile::Terminal(t) => {
            if t.shell.is_empty() {
                "login shell".into()
            } else {
                t.shell.clone()
            }
        }
    }
}
