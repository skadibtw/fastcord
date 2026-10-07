use crate::{Permissions, Snowflake};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Role {
    pub id: Snowflake,
    pub name: String,
    pub permissions: Permissions,
    #[serde(default)]
    pub position: i32,
}
