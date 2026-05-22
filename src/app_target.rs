use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTarget {
    pub bundle_id: String,
    pub name: String,
}
