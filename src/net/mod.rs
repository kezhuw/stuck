//! Networking primitives for TCP/UDP communication.
mod tcp;

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use ignore_result::Ignore;
use mio::event::{Event, Events};
use mio::{net, Interest, Token, Waker};
use slab::Slab;

pub use self::tcp::{TcpListener, TcpReader, TcpStream, TcpWriter};
use crate::channel::parallel;
use crate::channel::prelude::*;

const WAKER_TOKEN: Token = Token(usize::MAX);

enum Entry {
    Reader { readable: parallel::Sender<()> },
    Stream { readable: parallel::Sender<()>, writable: parallel::Sender<()> },
}

pub(crate) struct Registry {
    entries: Mutex<Slab<Entry>>,
    registry: mio::Registry,
    unused: Sender<Token>,
    freeing: Receiver<Token>,
}

// Safety: mpsc::Receiver is only used in net::poller thread.
unsafe impl Sync for Registry {}

impl Registry {
    fn new(poll: &mio::Poll) -> io::Result<Arc<Registry>> {
        let registry = poll.registry().try_clone()?;
        let (sender, receiver) = mpsc::channel();
        Ok(Arc::new(Registry { entries: Mutex::new(Slab::new()), registry, unused: sender, freeing: receiver }))
    }

    fn register_entry(&self, entry: Entry) -> Token {
        let mut entries = self.entries.lock().unwrap();
        let token = entries.insert(entry);
        Token(token)
    }

    fn unregister_entry(&self, token: Token) {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(token.0);
    }

    fn deregister_event_source(&self, token: Token, mut source: impl mio::event::Source) {
        self.registry.deregister(&mut source).ignore();
        // No events for this token will issued afterward from future poll,
        // but it is possible that net::poller is processing old events for this token.
        //
        // If we reclaim the token here, it is possible that it could be reused by new socket.
        // Let's hand over it to poller thread to ease contention evaluation.
        self.unused.send(token).ignore();
    }

    fn register_tcp_listener(&self, listener: &mut net::TcpListener) -> io::Result<(Token, parallel::Receiver<()>)> {
        let (readable_sender, readable_receiver) = parallel::bounded(2);
        let token = self.register_entry(Entry::Reader { readable: readable_sender });
        match self.registry.register(listener, token, Interest::READABLE) {
            Ok(_) => Ok((token, readable_receiver)),
            Err(err) => {
                self.unregister_entry(token);
                Err(err)
            },
        }
    }

    fn register_tcp_stream(
        &self,
        stream: &mut net::TcpStream,
    ) -> io::Result<(Token, parallel::Receiver<()>, parallel::Receiver<()>)> {
        let (readable_sender, readable_receiver) = parallel::bounded(2);
        let (writable_sender, writable_receiver) = parallel::bounded(2);
        let token = self.register_entry(Entry::Stream { readable: readable_sender, writable: writable_sender });
        match self.registry.register(stream, token, Interest::READABLE.add(Interest::WRITABLE)) {
            Ok(_) => Ok((token, readable_receiver, writable_receiver)),
            Err(err) => {
                self.unregister_entry(token);
                Err(err)
            },
        }
    }

    fn check_readable(readable: &mut parallel::Sender<()>, event: &Event) {
        if event.is_readable() || event.is_error() || event.is_read_closed() {
            readable.try_send(()).ignore();
        }
    }

    fn check_writable(writable: &mut parallel::Sender<()>, event: &Event) {
        if event.is_writable() || event.is_error() || event.is_write_closed() {
            writable.try_send(()).ignore();
        }
    }

    fn wake_events(&self, events: &mut Events) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let mut stopped = false;
        for event in events.iter() {
            let token = event.token();
            if token == WAKER_TOKEN {
                stopped = true;
            } else if let Some(entry) = entries.get_mut(token.0) {
                match entry {
                    Entry::Reader { readable } => {
                        Self::check_readable(readable, event);
                    },
                    Entry::Stream { readable, writable } => {
                        Self::check_readable(readable, event);
                        Self::check_writable(writable, event);
                    },
                }
            }
        }
        while let Ok(token) = self.freeing.try_recv() {
            entries.remove(token.0);
        }
        stopped
    }
}

pub(crate) struct Poller {
    poll: mio::Poll,
    registry: Arc<Registry>,
}

pub(crate) struct Stopper {
    waker: Waker,
    thread: Option<thread::JoinHandle<()>>,
}

impl Stopper {
    pub fn stop(&mut self) {
        self.waker.wake().unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let poll = mio::Poll::new()?;
        let registry = Registry::new(&poll)?;
        Ok(Poller { poll, registry })
    }

    pub fn start(mut self) -> io::Result<Stopper> {
        let waker = Waker::new(self.poll.registry(), WAKER_TOKEN)?;
        let handle = thread::Builder::new()
            .name("stuck::net::poller".to_string())
            .spawn(move || {
                self.serve().unwrap();
            })
            .expect("failt to spawn stuck::net::poller thread");
        Ok(Stopper { waker, thread: Some(handle) })
    }

    pub fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    fn serve(&mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(1024);
        let mut stopped = false;
        while !stopped {
            match self.poll.poll(&mut events, None) {
                Ok(_) => stopped = self.registry.wake_events(&mut events),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {},
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}
