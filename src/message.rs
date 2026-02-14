use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Common structure for JSON-RPC messages (for passthrough)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RpcId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcMessage {
    /// Check if this is a request
    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    /// Check if this is a notification
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }

    /// Check if this is a response
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }

    /// Get method name
    pub fn method_name(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// Create an error response for a given request
    pub fn error_response(request: &RpcMessage, message: &str) -> RpcMessage {
        RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            method: None,
            params: None,
            result: None,
            error: Some(RpcError {
                code: -32603,
                message: message.to_string(),
                data: None,
            }),
        }
    }
}
