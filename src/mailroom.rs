//! Structured, bounded cross-room message delivery.

use std::{collections::VecDeque, fmt::Write, io};

use crate::pane::Pane;

enum DeliveryStatus {
    Queued,
    Injected,
    Failed(String),
}

struct Envelope {
    id: u64,
    from: String,
    to: String,
    body: String,
    status: DeliveryStatus,
}

pub(crate) struct Mailroom {
    next_id: u64,
    capacity: usize,
    envelopes: VecDeque<Envelope>,
}

impl Mailroom {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            next_id: 1,
            capacity: capacity.max(1),
            envelopes: VecDeque::new(),
        }
    }

    pub(crate) fn deliver(
        &mut self,
        from: String,
        target: &mut Pane,
        body: String,
    ) -> (u64, io::Result<()>) {
        let id = self.queue(from, target.title().to_owned(), body);
        let result = self.inject(id, target);
        (id, result)
    }

    pub(crate) fn queue(&mut self, from: String, to: String, body: String) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.envelopes.push_back(Envelope {
            id,
            from,
            to,
            body,
            status: DeliveryStatus::Queued,
        });
        while self.envelopes.len() > self.capacity {
            self.envelopes.pop_front();
        }
        id
    }

    pub(crate) fn inject(&mut self, id: u64, target: &mut Pane) -> io::Result<()> {
        let (from, body) = self
            .envelopes
            .iter()
            .find(|item| item.id == id)
            .map(|item| (item.from.clone(), item.body.clone()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "envelope expired"))?;
        let result = target.send_whisper(&from, &body);
        let status = match &result {
            Ok(()) => DeliveryStatus::Injected,
            Err(error) => DeliveryStatus::Failed(error.to_string()),
        };
        self.set_status(id, status);
        result
    }

    pub(crate) fn len(&self) -> usize {
        self.envelopes.len()
    }

    pub(crate) fn summary(&self) -> String {
        if self.envelopes.is_empty() {
            return "No envelopes yet.".to_owned();
        }

        let mut summary = String::new();
        for envelope in self.envelopes.iter().rev() {
            let status = match &envelope.status {
                DeliveryStatus::Queued => "QUEUED".to_owned(),
                DeliveryStatus::Injected => "INJECTED".to_owned(),
                DeliveryStatus::Failed(error) => format!("FAILED: {error}"),
            };
            let _ = writeln!(
                summary,
                "#{:04}  {} → {}  [{}]\n  {}\n",
                envelope.id, envelope.from, envelope.to, status, envelope.body
            );
        }
        summary
    }

    fn set_status(&mut self, id: u64, status: DeliveryStatus) {
        if let Some(envelope) = self.envelopes.iter_mut().rev().find(|item| item.id == id) {
            envelope.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailroom_keeps_a_bounded_honest_history() {
        let mut mailroom = Mailroom::new(2);
        let first = mailroom.queue("A".into(), "B".into(), "one".into());
        mailroom.set_status(first, DeliveryStatus::Injected);
        let second = mailroom.queue("B".into(), "A".into(), "two".into());
        let third = mailroom.queue("A".into(), "B".into(), "three".into());
        mailroom.set_status(third, DeliveryStatus::Failed("offline".into()));

        assert_eq!(mailroom.len(), 2);
        let summary = mailroom.summary();
        assert!(!summary.contains("#0001"));
        assert!(summary.contains("#0002"));
        assert!(summary.contains("QUEUED"));
        assert!(summary.contains("#0003"));
        assert!(summary.contains("FAILED: offline"));
        assert_eq!(second, 2);
    }
}
