use std::cell::RefCell;
use std::net::TcpListener;
use std::ops::Not;
use std::os::fd::IntoRawFd;
use std::os::unix::io::RawFd;
use std::rc::Rc;

use crate::reactor::{EventReceiver, InterestAction, InterestActions, State, READ};
use crate::request_context::RequestContext;
use crate::{log, syscall};

fn set_nonblocking(fd: RawFd, nonblocking: bool) -> std::io::Result<()> {
    // The only difference of O_NONBLOCKING occurs when no data is present
    // and the write end is open. In this case, a // normal `read()` blocks,
    // while a nonblocking `read()` fails with `EAGAIN``
    let mut flags = syscall!(fcntl(fd, libc::F_GETFL))?;
    if nonblocking {
        flags |= libc::O_NONBLOCK;
    } else {
        flags &= libc::O_NONBLOCK.not();
    }
    syscall!(fcntl(fd, libc::F_SETFL, flags))?;
    Ok(())
}

pub struct Listener {
    fd: RawFd,
    verbose: bool,
    req_actor: Rc<RefCell<RequestContext>>,
}

impl Listener {
    pub(crate) fn new(
        verbose: bool,
        req_actor: Rc<RefCell<RequestContext>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:8000")?;
        let fd = listener.into_raw_fd();
        set_nonblocking(fd, true)?;
        Ok(Self {
            fd,
            verbose,
            req_actor,
        })
    }

    #[inline]
    pub(crate) fn raw_fd(&self) -> RawFd { self.fd }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.fd) };
    }
}

impl EventReceiver for Listener {
    fn on_ready(
        &mut self,
        ready_to: State,
        fd: RawFd,
        new_actions: &mut InterestActions,
    ) -> std::io::Result<()> {
        debug_assert!(ready_to.read());
        let accepted_socket = syscall!(accept(fd, std::ptr::null_mut(), std::ptr::null_mut()))?;
        if self.verbose {
            log(&format!("new client fd: {accepted_socket}"));
        }
        set_nonblocking(accepted_socket, true)?;
        new_actions.add(InterestAction::Add(
            accepted_socket,
            READ,
            self.req_actor.clone(),
        ));
        new_actions.add(InterestAction::Modify(fd, READ));
        Ok(())
    }
}
