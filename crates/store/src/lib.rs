//! Persistent state: the profile model, the SQLite profile store (profiles, buffers,
//! history, schema cache, settings, snippets, favorites, terminal macros) and secret storage (OS keychain or encrypted vault).

pub mod error;
pub mod favorites;
pub mod macros;
pub mod model;
pub mod paths;
pub mod random;
pub mod secrets;
pub mod snippets;
pub mod store;

pub use error::{Result, StoreError};
pub use favorites::Favorite;
pub use macros::Macro;
pub use model::{
    BufferState, DbConnection, EnvironmentLabel, FileConnection, FileProtocol, ForwardDirection,
    FtpMode, FtpTls, Host, HostPatch, PortForward, Profile, ProfileId, S3Auth, SecretRef, SshAuth,
    TerminalColors, TerminalProfile, ValidationError, Workspace,
};
pub use paths::AppPaths;
pub use secrets::{KeychainStore, MemoryStore, SecretStore, VaultStore};
pub use snippets::Snippet;
pub use store::{HistoryEntry, HistoryStatus, ProfileExport, Store, now_ms};
