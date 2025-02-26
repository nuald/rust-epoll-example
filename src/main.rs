use std::cell::RefCell;
use std::net::TcpListener;
use std::os::fd::IntoRawFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;

pub mod content_actor;
pub mod request_context;
pub mod signal;
pub mod timer;
pub mod reactor;
use crate::request_context::RequestContext;

use crate::reactor::READ_FLAGS;
use crate::reactor::Reactor;
use crate::reactor::InterestAction;
use crate::reactor::InterestActions;
use crate::reactor::EventReceiver;

#[macro_export]
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            let err = std::io::Error::last_os_error();
            Err(std::io::Error::new(err.kind(), format!("{}, {}:{}:{}", err, file!(), line!(), column!())))
        } else {
            Ok(res)
        }
    }};
}

#[cold]
fn log(msg: &str) {
    println!("{msg}");
}

struct RequestListener {
    listener: TcpListener,
    verbose: bool,
    req_actor: Rc<RefCell<RequestContext>>,
}

impl EventReceiver for RequestListener {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()> {
        match self.listener.accept() {
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
        new_actions.add(InterestAction::Modify(
            self.listener.as_raw_fd(),
            READ_FLAGS,
        ));
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> std::io::Result<()> {
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    let mut verbose = false;

    let args = std::env::args().skip(1);
    for arg in args {
        match &arg[..] {
            "-v" | "--verbose" => {
                verbose = true;
            }
            _ => {}
        }
    }

    let mut reactor = Reactor::new()?;
    let listener = TcpListener::bind("127.0.0.1:8000")?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();
    let content_handle = content_actor::Handle::new()?;
    let req_handle = request_context::Handle::new()?;
    let req_actor = req_handle.bind(&mut reactor, verbose, content_handle.clone())?;
    content_handle.bind(&mut reactor, verbose, req_handle)?;
    let listener = RequestListener {
        listener,
        verbose,
        req_actor,
    };
    reactor.add_interest(listener_fd, READ_FLAGS, Rc::new(RefCell::new(listener)))?;

    let signal_listener = signal::Listener::new()?;
    reactor.add_interest(
        signal_listener.fd,
        READ_FLAGS,
        Rc::new(RefCell::new(signal_listener)),
    )?;

    let timer_listener = timer::Listener::new()?;
    reactor.add_interest(timer_listener.fd, READ_FLAGS, Rc::new(RefCell::new(timer_listener)))?;

    reactor.run(verbose)?;
    println!("exited");
    Ok(())
}
