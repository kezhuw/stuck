use std::io::Result;

pub(crate) use super::pool::Requester;
use super::{pool, Stopper};
use crate::net::Registry;

pub(crate) struct Poller(pool::Poller);

impl Poller {
    pub fn new() -> (Self, Requester) {
        let (poller, requester) = pool::Poller::new();
        (Self(poller), requester)
    }

    pub fn start(self, _registry: &Registry) -> Result<Stopper> {
        let (stopper, _) = self.0.start(4)?;
        Ok(stopper)
    }
}
