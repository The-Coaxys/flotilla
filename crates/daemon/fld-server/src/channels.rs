//! Logical streams carry one-use capabilities issued by a live control session.
use fl_model::ClientSessionId;
use fl_proto::{CHANNEL_SCHEMA, ChannelGrant, ChannelHello, ChannelRole, RejectReason};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const LIFETIME: Duration = Duration::from_secs(30);
const MAX_SESSIONS: usize = 256;
const MAX_TICKETS: usize = 4096;
const MAX_SESSION_TICKETS: usize = 16;

pub struct Channels {
    state: Mutex<State>,
    lifetime: Duration,
}
impl Default for Channels {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            lifetime: LIFETIME,
        }
    }
}
#[derive(Default)]
struct State {
    controls: HashMap<[u8; 16], Control>,
    tickets: HashMap<[u8; 32], Ticket>,
}
struct Control {
    client: ClientSessionId,
    cancel: CancellationToken,
    roles: [Option<CancellationToken>; 3],
}
struct Ticket {
    session: [u8; 16],
    role: ChannelRole,
    expires: Instant,
}

pub struct ControlLease {
    registry: Arc<Channels>,
    pub id: [u8; 16],
    pub client: ClientSessionId,
    pub cancel: CancellationToken,
}
impl Drop for ControlLease {
    fn drop(&mut self) {
        self.cancel.cancel();
        let mut state = self
            .registry
            .state
            .lock()
            .expect("channel registry poisoned");
        state.controls.remove(&self.id);
        state.tickets.retain(|_, ticket| ticket.session != self.id);
    }
}
pub struct ChannelLease {
    pub client: ClientSessionId,
    pub session: [u8; 16],
    pub role: ChannelRole,
    pub cancel: CancellationToken,
}
impl Drop for ChannelLease {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Channels {
    pub fn register(
        self: &Arc<Self>,
        client: ClientSessionId,
        parent: &CancellationToken,
    ) -> std::io::Result<ControlLease> {
        let mut state = self.state.lock().expect("channel registry poisoned");
        if state.controls.len() >= MAX_SESSIONS {
            return Err(std::io::Error::other("control session capacity exhausted"));
        }
        let id = loop {
            let mut id = [0; 16];
            getrandom::fill(&mut id).map_err(|error| std::io::Error::other(error.to_string()))?;
            if !state.controls.contains_key(&id) {
                break id;
            }
        };
        let cancel = parent.child_token();
        state.controls.insert(
            id,
            Control {
                client,
                cancel: cancel.clone(),
                roles: [None, None, None],
            },
        );
        Ok(ControlLease {
            registry: self.clone(),
            id,
            client,
            cancel,
        })
    }

    pub fn grant(&self, session: [u8; 16], role: ChannelRole) -> std::io::Result<ChannelGrant> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("channel registry poisoned");
        state.tickets.retain(|_, ticket| ticket.expires > now);
        if !state
            .controls
            .get(&session)
            .is_some_and(|control| !control.cancel.is_cancelled())
        {
            return Err(std::io::Error::other("control session has ended"));
        }
        if state.tickets.len() >= MAX_TICKETS
            || state
                .tickets
                .values()
                .filter(|ticket| ticket.session == session)
                .count()
                >= MAX_SESSION_TICKETS
        {
            return Err(std::io::Error::other("channel grant capacity exhausted"));
        }
        let ticket = loop {
            let mut ticket = [0; 32];
            getrandom::fill(&mut ticket)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if !state.tickets.contains_key(&ticket) {
                break ticket;
            }
        };
        state.tickets.insert(
            ticket,
            Ticket {
                session,
                role,
                expires: now + self.lifetime,
            },
        );
        Ok(ChannelGrant {
            server_session_id: session,
            ticket,
            role,
            expires_in_ms: self.lifetime.as_millis().max(1) as u32,
        })
    }

    pub fn bind(&self, hello: &ChannelHello) -> Result<ChannelLease, RejectReason> {
        if hello.schema != CHANNEL_SCHEMA {
            return Err(RejectReason::UnsupportedChannelSchema);
        }
        let mut state = self.state.lock().expect("channel registry poisoned");
        let ticket = state
            .tickets
            .remove(&hello.ticket)
            .ok_or(RejectReason::InvalidCapability)?;
        // Session binding is checked before expiry: an expired ticket presented
        // with another session's identity is not a valid credential either.
        if ticket.session != hello.server_session_id {
            return Err(RejectReason::InvalidCapability);
        }
        if ticket.expires <= Instant::now() {
            return Err(RejectReason::ExpiredCapability);
        }
        if ticket.role != hello.role {
            return Err(RejectReason::WrongChannelRole);
        }
        let control = state
            .controls
            .get_mut(&ticket.session)
            .ok_or(RejectReason::InvalidCapability)?;
        if control.cancel.is_cancelled() {
            return Err(RejectReason::InvalidCapability);
        }
        let cancel = control.cancel.child_token();
        let index = match hello.role {
            ChannelRole::TerminalHot => 0,
            ChannelRole::TerminalBackground => 1,
            ChannelRole::Bulk => 2,
        };
        if let Some(old) = control.roles[index].replace(cancel.clone()) {
            old.cancel();
        }
        Ok(ChannelLease {
            client: control.client,
            session: ticket.session,
            role: hello.role,
            cancel,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(lifetime: Duration) -> Arc<Channels> {
        Arc::new(Channels {
            state: Mutex::default(),
            lifetime,
        })
    }

    #[test]
    fn expired_ticket_is_rejected_and_consumed() {
        let channels = registry(Duration::from_millis(20));
        let parent = CancellationToken::new();
        let control = channels.register(ClientSessionId::new(), &parent).unwrap();
        let grant = channels
            .grant(control.id, ChannelRole::TerminalHot)
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert!(matches!(
            channels.bind(&grant.hello()),
            Err(RejectReason::ExpiredCapability)
        ));
        assert!(matches!(
            channels.bind(&grant.hello()),
            Err(RejectReason::InvalidCapability)
        ));
    }

    #[test]
    fn role_replacement_cancels_previous_channel_and_control_end_cancels_all() {
        let channels = registry(LIFETIME);
        let parent = CancellationToken::new();
        let control = channels.register(ClientSessionId::new(), &parent).unwrap();
        let first = channels
            .bind(
                &channels
                    .grant(control.id, ChannelRole::Bulk)
                    .unwrap()
                    .hello(),
            )
            .unwrap();
        let second = channels
            .bind(
                &channels
                    .grant(control.id, ChannelRole::Bulk)
                    .unwrap()
                    .hello(),
            )
            .unwrap();
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled());
        let unused = channels
            .grant(control.id, ChannelRole::TerminalHot)
            .unwrap();
        drop(control);
        assert!(second.cancel.is_cancelled());
        assert!(matches!(
            channels.bind(&unused.hello()),
            Err(RejectReason::InvalidCapability)
        ));
    }
}
