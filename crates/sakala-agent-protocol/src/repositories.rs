use serde::{Deserialize, Serialize};

/// Access mode authorized by the control plane for one immutable repository checkout.
/// Credentials are deliberately not part of this serializable command contract.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAccess {
    #[default]
    Public,
    TemporaryCredential,
}
