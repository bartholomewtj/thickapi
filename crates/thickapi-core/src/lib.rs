pub mod adapters;
pub mod bootstrap;
pub mod config;
pub mod constants;
pub mod disk;
pub mod enforce;
pub mod engine;
pub mod env;
pub mod error;
pub mod lock;
pub mod pool;
pub mod process;
pub mod router;
pub mod store;
pub mod thin;
pub mod thinking;
pub mod trust;
pub mod types;

pub use bootstrap::bootstrap_config;
pub use config::{
    cli_bin_for_backend, default_config_dir, expand_path, load_config, load_config_with_legacy,
    required_cli_bins, ConfigBundle,
};
pub use constants::THICKAPI_CLAUDE_NS;
pub use disk::free_disk_bytes;
pub use enforce::Enforcer;
pub use engine::{
    read_events_jsonl, remember_turn_prompt, turn_artifact_dir, turn_prompt_conflicts, TurnEngine,
    TurnRegistry, TurnStatus,
};
pub use env::child_env;
pub use error::TurnError;
pub use lock::{InstanceLock, LockError, LOCK_FILE_NAME};
pub use pool::PoolManager;
pub use store::{account_block, account_health_is_blocking, AccountBlock, Store};
pub use thinking::{apply_thick, normalise_effort, profile, ThinkingProfile, VendorThinking};
pub use types::*;
