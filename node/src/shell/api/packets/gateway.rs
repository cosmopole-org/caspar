//! Request payloads for the `/gateway/*` bridge subscription actions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::input::IInput;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewaySubscribeInput {
    /// The bearer token the owning creature minted for this bridge.
    #[serde(default)]
    pub token: String,
    /// Topics to bind. Empty means "everything the grant covers"; a non-empty
    /// list can only narrow it.
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayUnsubscribeInput {
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewaySignalInput {
    #[serde(default)]
    pub token: String,
    /// Which of the grant's topics this call belongs to. Optional; when set it
    /// must be one the grant covers.
    #[serde(default)]
    pub topic: String,
    /// The creature action to invoke, e.g. `crew/message`.
    #[serde(default)]
    pub action: String,
    #[serde(rename = "correlationId", default)]
    pub correlation_id: String,
    #[serde(default)]
    pub payload: Value,
}

// A bridge is not a store member and never federates: these actions are
// served by the node the bridge connected to, which is the node that minted
// its grant, so both hooks are deliberately empty.
macro_rules! gateway_input {
    ($t:ty) => {
        impl IInput for $t {
            fn get_store_id(&self) -> String {
                String::new()
            }
            fn origin(&self) -> String {
                String::new()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
    };
}

gateway_input!(GatewaySubscribeInput);
gateway_input!(GatewayUnsubscribeInput);
gateway_input!(GatewaySignalInput);
