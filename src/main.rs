use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::io;
use std::io::prelude::*;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::collections::VecDeque;
use std::mem::MaybeUninit;

pub mod actor;

#[allow(unused_macros)]
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

trait EventReceiver {
    fn on_read(&mut self, new_actions: &mut InterestActions) -> io::Result<()>;
    fn on_write(&mut self, new_actions: &mut InterestActions) -> io::Result<()>;
}

#[derive(Debug)]
pub struct RequestContext {
    pub stream: TcpStream,
    pub content_length: usize,
    pub buf: Vec<u8>,
    verbose: bool,
}

impl RequestContext {
    fn new(stream: TcpStream, verbose: bool) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(32),
            content_length: 0,
            verbose,
        }
    }

    fn parse_and_set_content_length(&mut self, data: &str) {
        let content_length_slice = "content-length: ";
        let content_length_sz = content_length_slice.len();
        if data.contains("HTTP") {
            if let Some(content_length) = data.lines().find(|l| {
                l.len() > content_length_sz
                    && l[..content_length_sz].eq_ignore_ascii_case(content_length_slice)
            }) {
                self.content_length = content_length[content_length_sz..]
                    .parse::<usize>()
                    .expect("content-length is valid");
                if self.verbose {
                    log(&format!(
                        "set content length: {} bytes",
                        self.content_length
                    ));
                }
            }
        }
    }
}

impl EventReceiver for RequestContext {
    fn on_read(&mut self, new_actions: &mut InterestActions) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(_) => {
                if let Ok(data) = std::str::from_utf8(&buf) {
                    self.parse_and_set_content_length(data);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                return Err(e);
            }
        };
        self.buf.extend_from_slice(&buf);
        if self.buf.len() >= self.content_length {
            if self.verbose {
                log(&format!("got all data: {} bytes", self.buf.len()));
            }
            new_actions.add(InterestAction::Modify(self.stream.as_raw_fd(), WRITE_FLAGS));
        } else {
            new_actions.add(InterestAction::Modify(self.stream.as_raw_fd(), READ_FLAGS));
        }
        Ok(())
    }

    fn on_write(&mut self, new_actions: &mut InterestActions) -> io::Result<()> {
        if let Err(e) = self.stream.write_all(HTTP_RESP) {
            if self.verbose {
                log(&format!("could not answer: {e}"));
            }
        }
        new_actions.add(InterestAction::Remove(self.stream.as_raw_fd()));
        Ok(())
    }
}

const READ_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLIN;
const WRITE_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLOUT;

const HTTP_RESP: &[u8] = br"HTTP/1.1 200 OK
content-type: text/html
content-length: 5

Hello";

#[cold]
fn log(msg: &str) {
    println!("{msg}");
}

enum InterestAction {
    Add(RawFd, i32, Rc<RefCell<dyn EventReceiver>>),
    Modify(RawFd, i32),
    Remove(RawFd),
    Exit
}

struct InterestActions {
    actions: VecDeque<InterestAction>,
}

impl InterestActions {
    fn new() -> Self {
        Self {
            actions: VecDeque::new(),
        }
    }

    fn add(&mut self, action: InterestAction) {
        self.actions.push_back(action);
    }
}

impl Iterator for InterestActions {
    type Item = InterestAction;

    fn next(&mut self) -> Option<Self::Item> {
        self.actions.pop_front()
    }
}


pub struct Reactor {
    epoll_fd: RawFd,
    receivers: HashMap<RawFd, Rc<RefCell<dyn EventReceiver>>>,
}

impl Reactor {
    fn new() -> Self {
        let epoll_fd = epoll_create().expect("can create epoll queue");
        Self {
            epoll_fd,
            receivers: HashMap::new(),
        }
    }

    fn add_interest(
        &mut self,
        fd: RawFd,
        flags: i32,
        receiver: Rc<RefCell<dyn EventReceiver>>,
    ) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: flags as u32,
            u64: fd as u64,
        };
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_ADD,
            fd,
            &mut event
        ))?;
        self.receivers.insert(fd, receiver);
        Ok(())
    }

    fn modify_interest(&self, fd: RawFd, flags: i32) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: flags as u32,
            u64: fd as u64,
        };
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_MOD,
            fd,
            &mut event
        ))?;
        Ok(())
    }

    fn remove_interest(&mut self, fd: RawFd) -> io::Result<()> {
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_DEL,
            fd,
            std::ptr::null_mut()
        ))?;
        self.receivers.remove(&fd);
        Ok(())
    }

    fn apply(&mut self, actions: InterestActions) -> io::Result<bool> {
        let mut exit = false;
        for action in actions {
            match action {
                InterestAction::Add(fd, flags, receiver) => self.add_interest(fd, flags, receiver)?,
                InterestAction::Modify(fd, flags) => self.modify_interest(fd, flags)?,
                InterestAction::Remove(fd) => self.remove_interest(fd)?,
                InterestAction::Exit => {
                    exit = true;
                }
            }
        }
        Ok(exit)
    }

    fn run(&mut self, verbose: bool) -> io::Result<()> {
        let mut events: Vec<libc::epoll_event> = Vec::with_capacity(1024);
        loop {
            // TODO: avoid allocation in a loop
            let mut interest_actions = InterestActions::new();
            if verbose {
                log(&format!("receivers in flight: {}", self.receivers.len()));
            }
            events.clear();
            let res = match syscall!(epoll_wait(
                self.epoll_fd,
                events.as_mut_ptr(),
                1024,
                1000 as libc::c_int,
            )) {
                Ok(v) => v,
                Err(e) => panic!("error during epoll wait: {e}"),
            };

            #[allow(clippy::cast_sign_loss)]
            unsafe {
                events.set_len(res as usize);
            };

            for ev in &events {
                let fd = ev.u64 as RawFd;
                #[allow(clippy::cast_possible_wrap)]
                let events = ev.events as i32;
                match events {
                    v if v & libc::EPOLLIN == libc::EPOLLIN => match self.receivers.get(&fd) {
                        Some(receiver) => {
                            receiver.borrow_mut().on_read(&mut interest_actions)?;
                        }
                        None => {
                            if verbose {
                                log(&format!("unexpected fd {fd} for EPOLLIN"));
                            }
                        }
                    },
                    v if v & libc::EPOLLOUT == libc::EPOLLOUT => match self.receivers.get(&fd) {
                        Some(receiver) => {
                            receiver.borrow_mut().on_write(&mut interest_actions)?;
                        }
                        None => {
                            if verbose {
                                log(&format!("unexpected fd {fd} for EPOLLIN"));
                            }
                        }
                    },
                    v if v & libc::EPOLLOUT == libc::EPOLLOUT => {
                        self.remove_interest(fd)?;
                    }
                    v => {
                        if verbose {
                            log(&format!("unexpected events: {v}"));
                        }
                    }
                };
            }
            if self.apply(interest_actions)? {
                break Ok(());
            }
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        for (fd, _receiver) in self.receivers.drain() {
            // TODO: do we need on_unregister() callback
            // TODO: code duplication for syscall
            let _ = syscall!(epoll_ctl(
                self.epoll_fd,
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut()
            ));
        }
    }
}

struct RequestListener {
    listener: TcpListener,
    verbose: bool,
}

impl EventReceiver for RequestListener {
    fn on_read(&mut self, new_actions: &mut InterestActions) -> io::Result<()> {
        match self.listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(true)?;
                if self.verbose {
                    log(&format!("new client: {addr}"));
                }
                new_actions.add(InterestAction::Add(
                    stream.as_raw_fd(),
                    READ_FLAGS,
                    Rc::new(RefCell::new(RequestContext::new(stream, self.verbose))),
                ));
            }
            Err(e) => {
                if self.verbose {
                    log(&format!("couldn't accept: {e}"));
                }
            }
        };
        new_actions.add(InterestAction::Modify(self.listener.as_raw_fd(), READ_FLAGS));
        Ok(())
    }

    fn on_write(&mut self, _new_actions: &mut InterestActions) -> io::Result<()> {
        Ok(())
    }
}

struct SignalListener {
    signal_fd: RawFd
}

impl Drop for SignalListener {
    fn drop(&mut self) {
        // epoll receives EPOLLHUP upon file close,
        // so we don't need to manually drop it
        let _ = unsafe { libc::close(self.signal_fd) };
    }
}

impl EventReceiver for SignalListener {
    fn on_read(&mut self, new_actions: &mut InterestActions) -> io::Result<()> {
        new_actions.add(InterestAction::Exit);
        Ok(())
    }

    fn on_write(&mut self, _new_actions: &mut InterestActions) -> io::Result<()> {
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let mut verbose = false;

    let args = env::args().skip(1);
    for arg in args {
        match &arg[..] {
            "-v" | "--verbose" => {
                verbose = true;
            }
            _ => {}
        }
    }

    let mut reactor = Reactor::new();
    let listener = TcpListener::bind("127.0.0.1:8000")?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();
    let listener = RequestListener { listener, verbose };
    reactor.add_interest(listener_fd, READ_FLAGS, Rc::new(RefCell::new(listener)))?;

    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    syscall!(sigemptyset(mask.as_mut_ptr()))?;
    let mut mask = unsafe { mask.assume_init() };
    syscall!(sigaddset(&mut mask, libc::SIGINT))?;
    syscall!(sigprocmask(libc::SIG_BLOCK, &mut mask, std::ptr::null_mut()))?;
    let signal_fd = syscall!(signalfd(-1, &mask, 0))?;
    let signal_listener = SignalListener { signal_fd };
    reactor.add_interest(signal_fd, READ_FLAGS, Rc::new(RefCell::new(signal_listener)))?;

    //let actor_handle = actor::Handle::new(&mut reactor)?;
    reactor.run(verbose)?;
    println!("exited");
    Ok(())
}

fn epoll_create() -> io::Result<RawFd> {
    let fd = syscall!(epoll_create1(0))?;
    if let Ok(flags) = syscall!(fcntl(fd, libc::F_GETFD)) {
        let _ = syscall!(fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC));
    }

    Ok(fd)
}
