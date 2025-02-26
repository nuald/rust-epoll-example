use std::cell::RefCell;
use std::rc::Rc;

pub mod content_actor;
pub mod reactor;
pub mod request;
pub mod request_context;
pub mod signal;
pub mod timer;

use crate::reactor::EventReceiver;
use crate::reactor::InterestAction;
use crate::reactor::InterestActions;
use crate::reactor::Reactor;
use crate::reactor::READ_FLAGS;

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
    let content_handle = content_actor::Handle::new()?;
    let req_handle = request_context::Handle::new()?;
    let req_actor = req_handle.bind(&mut reactor, verbose, content_handle.clone())?;
    content_handle.bind(&mut reactor, verbose, req_handle)?;

    let listener = request::Listener::new(verbose, req_actor)?;
    reactor.add_interest(listener.fd, READ_FLAGS, Rc::new(RefCell::new(listener)))?;

    let signal_listener = signal::Listener::new()?;
    reactor.add_interest(
        signal_listener.fd,
        READ_FLAGS,
        Rc::new(RefCell::new(signal_listener)),
    )?;

    let timer_listener = timer::Listener::new()?;
    reactor.add_interest(
        timer_listener.fd,
        READ_FLAGS,
        Rc::new(RefCell::new(timer_listener)),
    )?;

    reactor.run(verbose)?;
    println!("exited");
    Ok(())
}
