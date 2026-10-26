use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombosConfig {
    #[serde(default)]
    pub combos: std::collections::BTreeMap<String, Vec<String>>,
}
