use crate::types::ResourceEvent;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub type EventSender = mpsc::Sender<ResourceEvent>;
pub type EventReceiver = mpsc::Receiver<ResourceEvent>;

/// Manages per-AIResource event channels
pub struct EventChannelRegistry {
    channels: HashMap<String, EventSender>,
}

impl EventChannelRegistry {
    pub fn new() -> Self {
        Self { channels: HashMap::new() }
    }

    pub fn register(&mut self, namespace: &str, name: &str) -> EventReceiver {
        let key = format!("{}/{}", namespace, name);
        let (tx, rx) = mpsc::channel(32);
        self.channels.insert(key, tx);
        rx
    }

    pub fn unregister(&mut self, namespace: &str, name: &str) {
        let key = format!("{}/{}", namespace, name);
        self.channels.remove(&key);
    }

    pub async fn send(&self, namespace: &str, name: &str, event: ResourceEvent) -> bool {
        let key = format!("{}/{}", namespace, name);
        if let Some(tx) = self.channels.get(&key) {
            tx.send(event).await.is_ok()
        } else {
            false
        }
    }

    pub fn has(&self, namespace: &str, name: &str) -> bool {
        let key = format!("{}/{}", namespace, name);
        self.channels.contains_key(&key)
    }
}
