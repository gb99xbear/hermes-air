use anyhow::Result;
use hermes_platform_telegram::TelegramAdapter;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, warn};

pub struct GatewayStreamConsumer {
    pub chat_id: i64,
    pub thread_id: Option<i64>,
    pub min_edit_interval: Duration,
    pub accumulated: String,
    pub message_id: Option<i64>,
    last_edit: Option<Instant>,
    last_edit_len: usize,
}

impl GatewayStreamConsumer {
    pub fn new(chat_id: i64) -> Self {
        Self {
            chat_id,
            thread_id: None,
            min_edit_interval: Duration::from_millis(800),
            accumulated: String::new(),
            message_id: None,
            last_edit: None,
            last_edit_len: 0,
        }
    }

    pub fn with_thread(mut self, thread_id: Option<i64>) -> Self {
        self.thread_id = thread_id;
        self
    }

    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_edit_interval = interval;
        self
    }

    pub async fn consume_stream(
        &mut self,
        mut rx: mpsc::Receiver<String>,
        tg_adapter: &TelegramAdapter,
        reply_to_id: Option<i64>,
    ) -> Result<String> {
        while let Some(chunk) = rx.recv().await {
            self.accumulated.push_str(&chunk);

            if self.message_id.is_none() {
                if !self.accumulated.trim().is_empty() {
                    match tg_adapter
                        .send_chunk_with_fallback_and_thread(self.chat_id, self.thread_id, &self.accumulated, reply_to_id)
                        .await
                    {
                        Ok(sent) => {
                            self.message_id = Some(sent.message_id);
                            self.last_edit = Some(Instant::now());
                            self.last_edit_len = self.accumulated.len();
                        }
                        Err(e) => {
                            error!("[stream_consumer] Initial chunk send failed: {:?}", e);
                        }
                    }
                }
                continue;
            }

            let should_edit = if let Some(last_time) = self.last_edit {
                last_time.elapsed() >= self.min_edit_interval
                    && (self.accumulated.len() >= self.last_edit_len + 10)
            } else {
                true
            };

            if should_edit {
                if let Some(msg_id) = self.message_id {
                    if let Err(e) = tg_adapter
                        .edit_message_text(self.chat_id, msg_id, &self.accumulated)
                        .await
                    {
                        warn!("[stream_consumer] Progressive edit failed: {:?}", e);
                    } else {
                        self.last_edit = Some(Instant::now());
                        self.last_edit_len = self.accumulated.len();
                    }
                }
            }
        }

        if self.accumulated.is_empty() {
            return Ok(self.accumulated.clone());
        }

        if let Some(msg_id) = self.message_id {
            if let Err(e) = tg_adapter
                .edit_message_text(self.chat_id, msg_id, &self.accumulated)
                .await
            {
                warn!("[stream_consumer] Final edit failed: {:?}", e);
            }
        } else {
            let _ = tg_adapter
                .send_chunk_with_fallback_and_thread(self.chat_id, self.thread_id, &self.accumulated, reply_to_id)
                .await;
        }

        Ok(self.accumulated.clone())
    }
}
