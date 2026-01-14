use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC メッセージの共通構造（パススルー用）
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
    /// リクエストかどうか
    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    /// 通知かどうか
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }

    /// レスポンスかどうか
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }

    /// メソッド名を取得
    pub fn method_name(&self) -> Option<&str> {
        self.method.as_deref()
    }
}
