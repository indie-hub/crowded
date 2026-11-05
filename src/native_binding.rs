//! Set-once session binding for a single room launch.
#![allow(dead_code)]

use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4},
};

use crate::native_outbox::Binding;

/// Evidence supplied by the owner of the server process after authenticated
/// server.info and explicit session creation or exact-session resume.
pub(crate) struct OwnershipEvidence {
    pub child_pid: u32,
    pub reported_pid: u32,
    pub endpoint: SocketAddrV4,
    pub session: String,
    pub generation: u64,
}

pub(crate) struct LaunchBinding {
    room: String,
    generation: u64,
    child_pid: u32,
    binding: Option<Binding>,
}

impl LaunchBinding {
    pub fn new(room: String, generation: u64, child_pid: u32) -> io::Result<Self> {
        if room.is_empty() || generation == 0 || child_pid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid launch identity",
            ));
        }
        Ok(Self {
            room,
            generation,
            child_pid,
            binding: None,
        })
    }

    pub fn bind(&mut self, evidence: OwnershipEvidence) -> io::Result<&Binding> {
        if self.binding.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "launch already bound",
            ));
        }
        if evidence.generation != self.generation
            || evidence.child_pid != self.child_pid
            || evidence.reported_pid != self.child_pid
            || *evidence.endpoint.ip() != Ipv4Addr::LOCALHOST
            || evidence.endpoint.port() == 0
            || evidence.session.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unverified session binding",
            ));
        }
        self.binding = Some(Binding {
            room: self.room.clone(),
            session: evidence.session,
            endpoint: format!("http://{}", evidence.endpoint),
            generation: self.generation,
        });
        Ok(self.binding.as_ref().unwrap())
    }

    pub fn accepts(&self, binding: &Binding) -> bool {
        self.binding.as_ref() == Some(binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn evidence(generation: u64, pid: u32, port: u16, session: &str) -> OwnershipEvidence {
        OwnershipEvidence {
            child_pid: pid,
            reported_pid: pid,
            endpoint: SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            session: session.into(),
            generation,
        }
    }

    #[test]
    fn bind_requires_owned_server_and_is_set_once() {
        let mut launch = LaunchBinding::new("room-a".into(), 3, 42).unwrap();
        let mut wrong_pid = evidence(3, 42, 1234, "session-a");
        wrong_pid.reported_pid = 43;
        assert!(launch.bind(wrong_pid).is_err());
        let mut wrong_child = evidence(3, 42, 1234, "session-a");
        wrong_child.child_pid = 44;
        assert!(launch.bind(wrong_child).is_err());
        assert!(launch.bind(evidence(2, 42, 1234, "session-a")).is_err());
        assert!(launch.bind(evidence(3, 42, 1234, "")).is_err());
        let mut wrong_host = evidence(3, 42, 1234, "session-a");
        wrong_host.endpoint = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234);
        assert!(launch.bind(wrong_host).is_err());
        let bound = launch
            .bind(evidence(3, 42, 1234, "session-a"))
            .unwrap()
            .clone();
        assert!(launch.bind(evidence(3, 42, 1234, "session-b")).is_err());
        assert!(launch.accepts(&bound));
        let mut stale = bound;
        stale.generation = 2;
        assert!(!launch.accepts(&stale));
    }

    #[test]
    fn rooms_have_distinct_bindings() {
        let mut a = LaunchBinding::new("room-a".into(), 1, 10).unwrap();
        let mut b = LaunchBinding::new("room-b".into(), 1, 11).unwrap();
        let a_binding = a.bind(evidence(1, 10, 1234, "session-a")).unwrap().clone();
        let b_binding = b.bind(evidence(1, 11, 1235, "session-b")).unwrap().clone();
        assert!(a.accepts(&a_binding));
        assert!(b.accepts(&b_binding));
        assert!(!a.accepts(&b_binding));
        assert!(!b.accepts(&a_binding));
    }
}
