//! OpenCode delivery through the durable outbox.
#![allow(dead_code)]

use std::{collections::HashMap, io, sync::Arc, time::Duration};

use sha2::{Digest, Sha256};

use crate::{
    native_outbox::{Adapter, AdapterResult, Binding, Message},
    opencode_native::{Delivery, Server},
};

pub(crate) struct OpenCodeAdapter {
    rooms: HashMap<String, (Binding, Arc<Server>)>,
    wait: Duration,
    #[cfg(test)]
    reply: Option<fn() -> std::sync::mpsc::Receiver<Delivery>>,
}

impl OpenCodeAdapter {
    pub fn new(rooms: Vec<(Binding, Arc<Server>)>) -> io::Result<Self> {
        let mut by_room = HashMap::new();
        for (binding, server) in rooms {
            if by_room
                .insert(binding.room.clone(), (binding, server))
                .is_some()
            {
                return Err(io::Error::other("duplicate native room"));
            }
        }
        Ok(Self {
            rooms: by_room,
            wait: Duration::from_secs(15),
            #[cfg(test)]
            reply: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_test_timeout(&mut self, timeout: Duration) {
        self.wait = timeout;
    }

    #[cfg(test)]
    pub(crate) fn set_test_reply(
        &mut self,
        timeout: Duration,
        reply: fn() -> std::sync::mpsc::Receiver<Delivery>,
    ) {
        self.wait = timeout;
        self.reply = Some(reply);
    }
}

// This mapping is permanent; the persisted Crowded ID fixes the native ID.
pub(crate) fn native_id(id: &str) -> Option<String> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(format!("msg_{id}"))
}

fn map_delivery(delivery: Delivery, id: String) -> AdapterResult {
    match delivery {
        Delivery::Accepted => AdapterResult::Accepted(Some(id)),
        Delivery::Conflict => {
            AdapterResult::Failed("OpenCode message ID conflicts with stored content".into())
        }
        Delivery::Rejected(reason) => AdapterResult::Failed(reason),
        Delivery::NotSent => AdapterResult::Dropped(
            "OpenCode request was not sent; inspect the binding before retry".into(),
        ),
        Delivery::Uncertain => AdapterResult::Uncertain,
    }
}

impl Adapter for OpenCodeAdapter {
    fn send(&self, message: Message) -> AdapterResult {
        let Some(id) = native_id(&message.id) else {
            return AdapterResult::Failed("invalid message ID".into());
        };
        if format!("{:x}", Sha256::digest(message.text.as_bytes())) != message.hash {
            return AdapterResult::Failed("message content hash mismatch".into());
        }
        let Some((binding, server)) = self.rooms.get(&message.destination) else {
            return AdapterResult::Dropped("native room is not bound".into());
        };
        if binding != &message.binding || message.destination != binding.room {
            return AdapterResult::Dropped(
                "native room binding changed; inspect before retry".into(),
            );
        }
        #[cfg(test)]
        let receiver = match self.reply {
            Some(reply) => reply(),
            None => server.queue_async(binding, id.clone(), message.text),
        };
        #[cfg(not(test))]
        let receiver = server.queue_async(binding, id.clone(), message.text);
        match receiver.recv_timeout(self.wait) {
            Ok(delivery) => map_delivery(delivery, id),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => AdapterResult::Uncertain,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => AdapterResult::Uncertain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_and_delivery_states() {
        let id = "a".repeat(64);
        assert_eq!(native_id(&id), Some(format!("msg_{id}")));
        assert_eq!(
            native_id("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .as_deref(),
            Some("msg_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert!(native_id(&"A".repeat(64)).is_none());
        assert!(native_id("msg_123").is_none());
        assert!(
            matches!(map_delivery(Delivery::Accepted, "id".into()), AdapterResult::Accepted(Some(id)) if id == "id")
        );
        assert!(matches!(
            map_delivery(Delivery::Conflict, "id".into()),
            AdapterResult::Failed(_)
        ));
        assert!(matches!(
            map_delivery(Delivery::Rejected("unauthorized".into()), "id".into()),
            AdapterResult::Failed(_)
        ));
        assert!(matches!(
            map_delivery(Delivery::NotSent, "id".into()),
            AdapterResult::Dropped(_)
        ));
        assert!(matches!(
            map_delivery(Delivery::Uncertain, "id".into()),
            AdapterResult::Uncertain
        ));
    }
}
