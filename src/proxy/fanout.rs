use crate::backend_pool::fanout_timeout;
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::state::PendingFanout;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::time::Instant;

impl super::LspProxy {
    /// Dispatch a fan-out request to all active backends.
    /// Each backend receives a copy of the request with a unique proxy ID.
    pub(crate) async fn dispatch_fanout_request(
        &mut self,
        msg: &RpcMessage,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let client_id = match &msg.id {
            Some(id) => id.clone(),
            None => return Ok(()), // notifications don't fan out
        };

        let backend_keys = self.state.pool.backends_keys();
        if backend_keys.is_empty() {
            let error_response = RpcMessage::error_response(
                msg,
                "lsp-proxy: no backends available for fan-out request",
            );
            client_writer.write_message(&error_response).await?;
            return Ok(());
        }

        let timeout = fanout_timeout();
        let deadline = if timeout.is_zero() {
            None // No timeout: wait forever
        } else {
            Some(Instant::now() + timeout)
        };

        let mut fanout = PendingFanout {
            client_request_id: client_id.clone(),
            expected_count: 0,
            results: Vec::new(),
            sub_requests: HashMap::new(),
            deadline,
            notified: false,
            failed_backends: Vec::new(),
            client_request: msg.clone(),
        };

        let mut total_dispatched = 0usize;

        for venv_path in &backend_keys {
            let proxy_id = self.state.alloc_proxy_request_id();

            // Clone the request with the proxy-assigned ID
            let mut sub_msg = msg.clone();
            sub_msg.id = Some(proxy_id.clone());

            let session = match self.state.pool.get(venv_path) {
                Some(inst) => inst.session,
                None => continue,
            };

            // Try to write to backend
            let write_ok = if let Some(inst) = self.state.pool.get_mut(venv_path) {
                inst.last_used = Instant::now();
                inst.writer.write_message(&sub_msg).await.is_ok()
            } else {
                false
            };

            if write_ok {
                fanout
                    .sub_requests
                    .insert(proxy_id.clone(), (venv_path.clone(), session));
                // Also register in pending_requests so stale-session checks work
                self.state.pending_requests.insert(
                    proxy_id,
                    crate::state::PendingRequest {
                        backend_session: session,
                        venv_path: venv_path.clone(),
                    },
                );
                total_dispatched += 1;
            } else {
                tracing::warn!(
                    venv = %venv_path.display(),
                    "Fan-out write failed, marking backend as failed"
                );
                fanout.failed_backends.push(venv_path.clone());
            }
        }

        if total_dispatched == 0 {
            // All backends failed to accept the write
            let error_response = RpcMessage::error_response(
                msg,
                "lsp-proxy: all backends failed to accept fan-out request",
            );
            client_writer.write_message(&error_response).await?;
            return Ok(());
        }

        fanout.expected_count = total_dispatched;

        tracing::info!(
            client_id = ?client_id,
            dispatched = total_dispatched,
            failed = fanout.failed_backends.len(),
            "Fan-out request dispatched"
        );

        self.state.pending_fanouts.insert(client_id, fanout);
        Ok(())
    }

    /// Handle a response from a backend that may be part of a fan-out.
    /// Returns `true` if the response was consumed by a fan-out (caller should skip normal handling).
    pub(crate) async fn handle_fanout_response(
        &mut self,
        response_id: &RpcId,
        msg: &RpcMessage,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<bool, ProxyError> {
        // Find which fanout owns this response_id
        let client_id = self
            .state
            .pending_fanouts
            .iter()
            .find(|(_, f)| f.sub_requests.contains_key(response_id))
            .map(|(cid, _)| cid.clone());

        let client_id = match client_id {
            Some(id) => id,
            None => return Ok(false), // not a fan-out response
        };

        // Remove the sub-request entry
        let fanout = self.state.pending_fanouts.get_mut(&client_id).unwrap();
        let (venv_path, _session) = fanout.sub_requests.remove(response_id).unwrap();

        // Clean up from pending_requests
        self.state.pending_requests.remove(response_id);

        // Process the response
        if msg.error.is_some() {
            fanout.failed_backends.push(venv_path);
        } else if let Some(result) = &msg.result {
            // workspace/symbol returns an array of SymbolInformation
            if let Some(arr) = result.as_array() {
                fanout.results.extend(arr.iter().cloned());
            }
            // null result = no symbols found, that's fine
        }

        fanout.expected_count = fanout.expected_count.saturating_sub(1);

        if fanout.expected_count == 0 {
            let fanout = self.state.pending_fanouts.remove(&client_id).unwrap();
            self.complete_fanout(fanout, client_writer).await?;
        }

        Ok(true)
    }

    /// Complete a fan-out: deduplicate and send merged results to the client.
    pub(crate) async fn complete_fanout(
        &self,
        fanout: PendingFanout,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if fanout.results.is_empty() && !fanout.failed_backends.is_empty() {
            // All backends failed, no results at all
            let error_response = RpcMessage::error_response(
                &fanout.client_request,
                &format!(
                    "lsp-proxy: all backends failed for fan-out request ({})",
                    fanout
                        .failed_backends
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            client_writer.write_message(&error_response).await?;
        } else {
            let deduped = dedupe_symbol_results(fanout.results);
            let response = RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(fanout.client_request_id),
                method: None,
                params: None,
                result: Some(serde_json::Value::Array(deduped)),
                error: None,
            };
            client_writer.write_message(&response).await?;
        }
        Ok(())
    }

    /// Expire fan-out requests that have passed their deadline.
    /// Sends partial results and a warning notification.
    pub(crate) async fn expire_fanout_requests(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let now = Instant::now();
        let expired_ids: Vec<RpcId> = self
            .state
            .pending_fanouts
            .iter()
            .filter(|(_, f)| f.deadline.is_some_and(|d| now >= d))
            .map(|(id, _)| id.clone())
            .collect();

        for client_id in expired_ids {
            let mut fanout = self.state.pending_fanouts.remove(&client_id).unwrap();

            // Collect timed-out backend info for the warning message
            let timed_out_venvs: Vec<String> = fanout
                .sub_requests
                .values()
                .map(|(venv, _)| venv.display().to_string())
                .collect();

            // Send $/cancelRequest to remaining backends (best effort)
            for (proxy_id, (venv_path, _session)) in &fanout.sub_requests {
                self.state.pending_requests.remove(proxy_id);
                let cancel_msg = RpcMessage::notification(
                    "$/cancelRequest",
                    Some(serde_json::json!({ "id": proxy_id })),
                );
                if let Some(inst) = self.state.pool.get_mut(venv_path) {
                    let _ = inst.writer.write_message(&cancel_msg).await;
                }
            }

            // Record timed-out backends as failed
            for (_, (venv_path, _)) in std::mem::take(&mut fanout.sub_requests) {
                fanout.failed_backends.push(venv_path);
            }

            // Send warning notification (max 1 per fan-out, check `notified` flag)
            if !fanout.notified && !timed_out_venvs.is_empty() {
                fanout.notified = true;
                let warn_msg = RpcMessage::notification(
                    "window/showMessage",
                    Some(serde_json::json!({
                        "type": 2, // Warning
                        "message": format!(
                            "typemux-cc: fan-out timeout, partial results returned. Timed out backends: {}",
                            timed_out_venvs.join(", ")
                        )
                    })),
                );
                let _ = client_writer.write_message(&warn_msg).await;
            }

            tracing::warn!(
                client_id = ?client_id,
                timed_out = ?timed_out_venvs,
                results_count = fanout.results.len(),
                "Fan-out request timed out, returning partial results"
            );

            fanout.expected_count = 0;
            self.complete_fanout(fanout, client_writer).await?;
        }

        Ok(())
    }

    /// Cancel a fan-out request (e.g., client sent $/cancelRequest for it).
    pub(crate) async fn cancel_fanout_request(
        &mut self,
        client_id: &RpcId,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if let Some(fanout) = self.state.pending_fanouts.remove(client_id) {
            // Send $/cancelRequest to all remaining backends
            for (proxy_id, (venv_path, _session)) in &fanout.sub_requests {
                self.state.pending_requests.remove(proxy_id);
                let cancel_msg = RpcMessage::notification(
                    "$/cancelRequest",
                    Some(serde_json::json!({ "id": proxy_id })),
                );
                if let Some(inst) = self.state.pool.get_mut(venv_path) {
                    let _ = inst.writer.write_message(&cancel_msg).await;
                }
            }

            // Send cancelled response to client
            let response =
                RpcMessage::cancelled_response(client_id.clone(), "Fan-out request cancelled");
            client_writer.write_message(&response).await?;

            tracing::info!(
                client_id = ?client_id,
                "Fan-out request cancelled"
            );
        }
        Ok(())
    }

    /// Cancel fan-out sub-requests for a specific backend (venv + session).
    /// Called when a backend crashes or is evicted.
    /// Returns `client_ids` of affected fan-outs that need convergence checks.
    pub(crate) fn cancel_fanout_sub_requests(
        &mut self,
        venv_path: &PathBuf,
        session: u64,
    ) -> Vec<RpcId> {
        let mut affected_client_ids = Vec::new();

        for (client_id, fanout) in &mut self.state.pending_fanouts {
            let matching_proxy_ids: Vec<RpcId> = fanout
                .sub_requests
                .iter()
                .filter(|(_, (v, s))| v == venv_path && *s == session)
                .map(|(pid, _)| pid.clone())
                .collect();

            if matching_proxy_ids.is_empty() {
                continue;
            }

            for proxy_id in &matching_proxy_ids {
                fanout.sub_requests.remove(proxy_id);
                self.state.pending_requests.remove(proxy_id);
                fanout.expected_count = fanout.expected_count.saturating_sub(1);
            }
            fanout.failed_backends.push(venv_path.clone());

            if fanout.expected_count == 0 {
                affected_client_ids.push(client_id.clone());
            }
        }

        affected_client_ids
    }
}

/// Deduplicate workspace/symbol results.
/// Key: (uri, range.start.line, range.start.character, name, kind)
/// Items with missing fields are kept (defensive).
pub fn dedupe_symbol_results(results: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(results.len());

    for item in results {
        let key = extract_dedupe_key(&item);
        match key {
            Some(k) => {
                if seen.insert(k) {
                    deduped.push(item);
                }
            }
            None => {
                // Missing fields: keep defensively
                deduped.push(item);
            }
        }
    }

    deduped
}

/// Extract dedup key from a `SymbolInformation` value.
fn extract_dedupe_key(item: &serde_json::Value) -> Option<(String, u64, u64, String, u64)> {
    let name = item.get("name")?.as_str()?;
    let kind = item.get("kind")?.as_u64()?;
    let location = item.get("location")?;
    let uri = location.get("uri")?.as_str()?;
    let range = location.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()?;
    let character = start.get("character")?.as_u64()?;

    Some((uri.to_string(), line, character, name.to_string(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_dedupe_removes_duplicates() {
        let results = vec![
            json!({
                "name": "MyClass",
                "kind": 5,
                "location": {
                    "uri": "file:///a.py",
                    "range": {"start": {"line": 10, "character": 0}, "end": {"line": 10, "character": 7}}
                }
            }),
            json!({
                "name": "MyClass",
                "kind": 5,
                "location": {
                    "uri": "file:///a.py",
                    "range": {"start": {"line": 10, "character": 0}, "end": {"line": 10, "character": 7}}
                }
            }),
            json!({
                "name": "OtherClass",
                "kind": 5,
                "location": {
                    "uri": "file:///b.py",
                    "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 10}}
                }
            }),
        ];

        let deduped = dedupe_symbol_results(results);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0]["name"], "MyClass");
        assert_eq!(deduped[1]["name"], "OtherClass");
    }

    #[test]
    fn test_dedupe_keeps_items_with_missing_fields() {
        let results = vec![
            json!({"name": "Incomplete"}), // no location
            json!({
                "name": "Complete",
                "kind": 5,
                "location": {
                    "uri": "file:///a.py",
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 8}}
                }
            }),
        ];

        let deduped = dedupe_symbol_results(results);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn test_dedupe_empty_input() {
        let deduped = dedupe_symbol_results(vec![]);
        assert!(deduped.is_empty());
    }

    #[test]
    fn test_dedupe_different_locations_same_name() {
        let results = vec![
            json!({
                "name": "foo",
                "kind": 12,
                "location": {
                    "uri": "file:///a.py",
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}
                }
            }),
            json!({
                "name": "foo",
                "kind": 12,
                "location": {
                    "uri": "file:///b.py",
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}
                }
            }),
        ];

        let deduped = dedupe_symbol_results(results);
        assert_eq!(deduped.len(), 2); // different URIs = different symbols
    }
}
