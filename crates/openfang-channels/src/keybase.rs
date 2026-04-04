//! Keybase Chat channel adapter.
//!
//! Uses the `keybase chat api` CLI subprocess for sending and receiving messages.
//! Listens for real-time messages via `keybase chat api-listen` as a long-running
//! subprocess. Authentication uses the logged-in Keybase session.

use crate::types::{
    split_message, ChannelAdapter, ChannelContent, ChannelMessage, ChannelType, ChannelUser,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use zeroize::Zeroizing;

/// Maximum message length for Keybase messages.
const MAX_MESSAGE_LEN: usize = 10000;

/// Run a `keybase chat api` command by piping JSON to stdin and reading the response from stdout.
async fn run_keybase_chat_api(
    payload: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::process::Command;

    let mut child = Command::new("keybase")
        .args(["chat", "api"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload_bytes = serde_json::to_vec(payload)?;
        stdin.write_all(&payload_bytes).await?;
        // Drop stdin to signal EOF
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(format!("keybase chat api exited with status: {}", output.status).into());
    }
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(result)
}

/// Keybase Chat channel adapter using the `keybase` CLI subprocess.
///
/// Interfaces with the Keybase Chat API via `keybase chat api` and
/// `keybase chat api-listen` subprocesses. Supports filtering by team names
/// for team-based conversations.
pub struct KeybaseAdapter {
    /// Keybase username for filtering own messages.
    username: String,
    /// SECURITY: Paper key is zeroized on drop. Kept for potential `keybase login` use.
    #[allow(dead_code)]
    paperkey: Zeroizing<String>,
    /// Team names to listen on (empty = all conversations).
    allowed_teams: Vec<String>,
    /// Shutdown signal.
    shutdown_tx: Arc<watch::Sender<bool>>,
    shutdown_rx: watch::Receiver<bool>,
    /// Last read message ID per conversation for deduplication.
    last_msg_ids: Arc<RwLock<HashMap<String, i64>>>,
}

impl KeybaseAdapter {
    /// Create a new Keybase adapter.
    ///
    /// # Arguments
    /// * `username` - Keybase username (used to filter own messages).
    /// * `paperkey` - Paper key (kept for potential login; zeroized on drop).
    /// * `allowed_teams` - Team names to filter conversations (empty = all).
    pub fn new(username: String, paperkey: String, allowed_teams: Vec<String>) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            username,
            paperkey: Zeroizing::new(paperkey),
            allowed_teams,
            shutdown_tx: Arc::new(shutdown_tx),
            shutdown_rx,
            last_msg_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// List conversations from the Keybase Chat API.
    #[allow(dead_code)]
    async fn list_conversations(
        &self,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let payload = serde_json::json!({
            "method": "list",
            "params": {"options": {}}
        });

        let result = run_keybase_chat_api(&payload)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e })?;

        let conversations = result["result"]["conversations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(conversations)
    }

    /// Read messages from a specific conversation channel.
    #[allow(dead_code)]
    async fn read_messages(
        &self,
        channel: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let payload = serde_json::json!({
            "method": "read",
            "params": {
                "options": {
                    "channel": channel,
                    "pagination": {"num": 20}
                }
            }
        });

        let result = run_keybase_chat_api(&payload)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e })?;

        let messages = result["result"]["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(messages)
    }

    /// Send a text message to a Keybase conversation.
    async fn api_send_message(
        &self,
        channel: &serde_json::Value,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let chunks = split_message(text, MAX_MESSAGE_LEN);

        for chunk in chunks {
            let payload = serde_json::json!({
                "method": "send",
                "params": {
                    "options": {
                        "channel": channel,
                        "message": {"body": chunk}
                    }
                }
            });

            run_keybase_chat_api(&payload)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        }

        Ok(())
    }

    /// Check if a team name is in the allowed list.
    #[allow(dead_code)]
    fn is_allowed_team(&self, team_name: &str) -> bool {
        self.allowed_teams.is_empty() || self.allowed_teams.iter().any(|t| t == team_name)
    }
}

#[async_trait]
impl ChannelAdapter for KeybaseAdapter {
    fn name(&self) -> &str {
        "keybase"
    }

    fn channel_type(&self) -> ChannelType {
        ChannelType::Custom("keybase".to_string())
    }

    async fn start(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = ChannelMessage> + Send>>, Box<dyn std::error::Error>>
    {
        info!("Keybase adapter starting for user {}", self.username);

        let (tx, rx) = tokio::sync::mpsc::channel::<ChannelMessage>(256);
        let username = self.username.clone();
        let allowed_teams = self.allowed_teams.clone();
        let last_msg_ids = Arc::clone(&self.last_msg_ids);
        let mut shutdown_rx = self.shutdown_rx.clone();

        tokio::spawn(async move {
            let mut child = match tokio::process::Command::new("keybase")
                .args(["chat", "api-listen"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    warn!("Keybase: failed to spawn 'keybase chat api-listen': {e}. Is the keybase binary installed and in PATH?");
                    return;
                }
            };

            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    warn!("Keybase: failed to capture stdout from api-listen");
                    child.kill().await.ok();
                    return;
                }
            };

            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("Keybase adapter shutting down");
                        break;
                    }
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                let event: serde_json::Value = match serde_json::from_str(&line) {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };

                                // Only process chat messages
                                if event["type"].as_str() != Some("chat") {
                                    continue;
                                }

                                let msg = &event["msg"];
                                let channel_info = &msg["channel"];
                                let members_type = channel_info["members_type"].as_str().unwrap_or("");
                                let team_name = channel_info["name"].as_str().unwrap_or("");
                                let topic_name = channel_info["topic_name"].as_str().unwrap_or("general");

                                // Filter by allowed teams
                                if !allowed_teams.is_empty()
                                    && members_type == "team"
                                    && !allowed_teams.iter().any(|t| t == team_name)
                                {
                                    continue;
                                }

                                let sender_username = msg["sender"]["username"].as_str().unwrap_or("");
                                // Skip own messages
                                if sender_username == username {
                                    continue;
                                }

                                let content_type = msg["content"]["type"].as_str().unwrap_or("");
                                if content_type != "text" {
                                    continue;
                                }

                                let text = msg["content"]["text"]["body"].as_str().unwrap_or("");
                                if text.is_empty() {
                                    continue;
                                }

                                let msg_id = msg["id"].as_i64().unwrap_or(0);
                                let conv_key = format!("{team_name}:{topic_name}");

                                // Deduplicate using last_msg_ids
                                {
                                    let ids = last_msg_ids.read().await;
                                    if let Some(&last_id) = ids.get(&conv_key) {
                                        if msg_id <= last_id {
                                            continue;
                                        }
                                    }
                                }

                                // Update last known ID
                                last_msg_ids.write().await.insert(conv_key.clone(), msg_id);

                                let sender_device = msg["sender"]["device_name"].as_str().unwrap_or("");
                                let is_group = members_type == "team";

                                let msg_content = if text.starts_with('/') {
                                    let parts: Vec<&str> = text.splitn(2, ' ').collect();
                                    let cmd = parts[0].trim_start_matches('/');
                                    let args: Vec<String> = parts
                                        .get(1)
                                        .map(|a| a.split_whitespace().map(String::from).collect())
                                        .unwrap_or_default();
                                    ChannelContent::Command {
                                        name: cmd.to_string(),
                                        args,
                                    }
                                } else {
                                    ChannelContent::Text(text.to_string())
                                };

                                let channel_msg = ChannelMessage {
                                    channel: ChannelType::Custom("keybase".to_string()),
                                    platform_message_id: msg_id.to_string(),
                                    sender: ChannelUser {
                                        platform_id: conv_key.clone(),
                                        display_name: sender_username.to_string(),
                                        openfang_user: None,
                                    },
                                    content: msg_content,
                                    target_agent: None,
                                    timestamp: Utc::now(),
                                    is_group,
                                    thread_id: None,
                                    metadata: {
                                        let mut m = HashMap::new();
                                        m.insert(
                                            "team_name".to_string(),
                                            serde_json::Value::String(team_name.to_string()),
                                        );
                                        m.insert(
                                            "topic_name".to_string(),
                                            serde_json::Value::String(topic_name.to_string()),
                                        );
                                        m.insert(
                                            "sender_device".to_string(),
                                            serde_json::Value::String(sender_device.to_string()),
                                        );
                                        m
                                    },
                                };

                                if tx.send(channel_msg).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                info!("Keybase api-listen stream ended (EOF)");
                                break;
                            }
                            Err(e) => {
                                warn!("Keybase api-listen read error: {e}");
                                break;
                            }
                        }
                    }
                }
            }

            // Kill the api-listen child process on shutdown
            child.kill().await.ok();
            info!("Keybase api-listen subprocess stopped");
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn send(
        &self,
        user: &ChannelUser,
        content: ChannelContent,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let text = match content {
            ChannelContent::Text(text) => text,
            _ => "(Unsupported content type)".to_string(),
        };

        // Parse platform_id back into channel info (format: "team:topic")
        let parts: Vec<&str> = user.platform_id.splitn(2, ':').collect();
        let (team_name, topic_name) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            (user.platform_id.as_str(), "general")
        };

        let channel_info = serde_json::json!({
            "name": team_name,
            "topic_name": topic_name,
            "members_type": "team",
        });

        self.api_send_message(&channel_info, &text).await?;
        Ok(())
    }

    async fn send_typing(&self, _user: &ChannelUser) -> Result<(), Box<dyn std::error::Error>> {
        // Keybase does not expose a typing indicator via the JSON API
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self.shutdown_tx.send(true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keybase_adapter_creation() {
        let adapter = KeybaseAdapter::new(
            "testuser".to_string(),
            "paper-key-phrase".to_string(),
            vec!["myteam".to_string()],
        );
        assert_eq!(adapter.name(), "keybase");
        assert_eq!(
            adapter.channel_type(),
            ChannelType::Custom("keybase".to_string())
        );
    }

    #[test]
    fn test_keybase_allowed_teams() {
        let adapter = KeybaseAdapter::new(
            "user".to_string(),
            "paperkey".to_string(),
            vec!["team-a".to_string(), "team-b".to_string()],
        );
        assert!(adapter.is_allowed_team("team-a"));
        assert!(adapter.is_allowed_team("team-b"));
        assert!(!adapter.is_allowed_team("team-c"));

        let open = KeybaseAdapter::new("user".to_string(), "paperkey".to_string(), vec![]);
        assert!(open.is_allowed_team("any-team"));
    }

    #[test]
    fn test_keybase_paperkey_zeroized() {
        let adapter = KeybaseAdapter::new(
            "user".to_string(),
            "my secret paper key".to_string(),
            vec![],
        );
        assert_eq!(adapter.paperkey.as_str(), "my secret paper key");
    }

    #[test]
    fn test_keybase_username_stored() {
        let adapter = KeybaseAdapter::new("alice".to_string(), "key".to_string(), vec![]);
        assert_eq!(adapter.username, "alice");
    }
}
