use std::cell::RefCell;
use std::net::TcpListener;
use std::os::fd::IntoRawFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;

use crate::log;
use crate::reactor::EventReceiver;
use crate::reactor::InterestAction;
use crate::reactor::InterestActions;
use crate::reactor::READ_FLAGS;
use crate::reactor::State;
use crate::request_context::RequestContext;

pub struct Listener {
    pub file: TcpListener,
    verbose: bool,
    req_actor: Rc<RefCell<RequestContext>>,
}

impl Listener {
    pub(crate) fn new(
        verbose: bool,
        req_actor: Rc<RefCell<RequestContext>>,
    ) -> std::io::Result<Self> {
        let file = TcpListener::bind("127.0.0.1:8000")?;
        file.set_nonblocking(true)?;
        Ok(Self {
            file,
            verbose,
            req_actor,
        })
    }
}

impl EventReceiver for Listener {
    fn on_ready(&mut self, ready_to: State, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()> {
        debug_assert!(ready_to.read());
        match self.file.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(true)?;
                if self.verbose {
                    log(&format!("new client: {addr}"));
                }
                new_actions.add(InterestAction::Add(
                    stream.into_raw_fd(),
                    READ_FLAGS,
                    self.req_actor.clone(),
                ));
            }
            Err(e) => {
                if self.verbose {
                    log(&format!("couldn't accept: {e}"));
                }
            }
        };
        new_actions.add(InterestAction::Modify(self.file.as_raw_fd(), READ_FLAGS));
        Ok(())
    }
}
