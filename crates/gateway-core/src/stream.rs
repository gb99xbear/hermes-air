use anyhow::Result;
use hermes_platform_telegram::TelegramAdapter;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, warn};

pub struct GatewayStreamConsumer {
    pub chat_id: i64,
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
            min_edit_interval: Duration::from_millis(800),
            accumulated: String::new(),
            message_id: None,
            last_edit: None,
            last_edit_len: 0,
        }
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
                        .send_chunk_with_fallback(self.chat_id, &self.accumulated, reply_to_id)
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
            match tg_adapter
                .send_chunk_with_fallback(self.chat_id, &self.accumulated, reply_to_id)
                .await
            {
                Ok(sent) => {
                    self.message_id = Some(sent.message_id);
                }
                Err(e) => {
                    error!("[stream_consumer] Final send chunk failed: {:?}", e);
                }
            }
        }

        Ok(self.accumulated.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stream_buffering_and_channel_drain() {
        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(async move {
            tx.send("Hello ".to_string()).await.unwrap();
            tx.send("from ".to_string()).await.unwrap();
            tx.send("stream!".to_string()).await.unwrap();
        });
        let mut acc = String::new();
        while let Some(chunk) = rx.recv().await {
            acc.push_str(&chunk);
        }
        assert_eq!(acc, "Hello from stream!");
    }

    #[test]
    fn test_stream_consumer_initialization() {
        let consumer = GatewayStreamConsumer::new(12345);
        assert_eq!(consumer.chat_id, 12345);
        assert_eq!(consumer.min_edit_interval, Duration::from_millis(800));
        assert_eq!(consumer.accumulated, "");
        assert_eq!(consumer.message_id, None);
    }

    #[test]
    fn test_stream_consumer_custom_interval() {
        let consumer = GatewayStreamConsumer::new(67890)
            .with_min_interval(Duration::from_millis(1500));
        assert_eq!(consumer.min_edit_interval, Duration::from_millis(1500));
    }

    #[test]
    fn test_throttling_decision_logic() {
        let mut consumer = GatewayStreamConsumer::new(100)
            .with_min_interval(Duration::from_millis(100));
        consumer.last_edit = Some(Instant::now());
        consumer.last_edit_len = 5;
        consumer.accumulated = "1234567".to_string(); // only +2 chars
        let should_edit = consumer.last_edit.unwrap().elapsed() >= consumer.min_edit_interval
            && (consumer.accumulated.len() >= consumer.last_edit_len + 10);
        assert!(!should_edit);

        // +10 chars
        consumer.accumulated = "123456789012345".to_string();
        // simulate time passing
        consumer.last_edit = Some(Instant::now() - Duration::from_millis(150));
        let should_edit_now = consumer.last_edit.unwrap().elapsed() >= consumer.min_edit_interval
            && (consumer.accumulated.len() >= consumer.last_edit_len + 10);
        assert!(should_edit_now);
    }
}
