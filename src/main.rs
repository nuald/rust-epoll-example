use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::mem::MaybeUninit;
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::os::fd::IntoRawFd;

pub mod content_actor;
pub mod request_context;
use crate::request_context::RequestContext;

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

trait EventReceiver {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()>;
    fn on_write(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()>;
}

const READ_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLIN;
const WRITE_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLOUT;

#[cold]
fn log(msg: &str) {
    println!("{msg}");
}

enum InterestAction {
    Add(RawFd, i32, Rc<RefCell<dyn EventReceiver>>),
    Modify(RawFd, i32),
    Remove(RawFd),
    Exit,
    PrintStats,
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
                InterestAction::Add(fd, flags, receiver) => {
                    self.add_interest(fd, flags, receiver)?
                }
                InterestAction::Modify(fd, flags) => self.modify_interest(fd, flags)?,
                InterestAction::Remove(fd) => self.remove_interest(fd)?,
                InterestAction::Exit => {
                    exit = true;
                }
                InterestAction::PrintStats => {
                    log(&format!("receivers in flight: {}", self.receivers.len()));
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
            events.clear();
            let res = match syscall!(epoll_wait(self.epoll_fd, events.as_mut_ptr(), 1024, -1,)) {
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
                            receiver.borrow_mut().on_read(fd, &mut interest_actions)?;
                        }
                        None => {
                            if verbose {
                                log(&format!("unexpected fd {fd} for EPOLLIN"));
                            }
                        }
                    },
                    v if v & libc::EPOLLOUT == libc::EPOLLOUT => match self.receivers.get(&fd) {
                        Some(receiver) => {
                            receiver.borrow_mut().on_write(fd, &mut interest_actions)?;
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
    req_actor: Rc<RefCell<RequestContext>>,
}

impl EventReceiver for RequestListener {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
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

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> io::Result<()> {
        Ok(())
    }
}

struct SignalListener {
    signal: File,
}

impl SignalListener {
    fn new(signal_fd: RawFd) -> Self {
        Self {
            signal: unsafe { File::from_raw_fd(signal_fd) },
        }
    }
}

impl EventReceiver for SignalListener {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        new_actions.add(InterestAction::Exit);
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> io::Result<()> {
        Ok(())
    }
}

struct TimerListener {
    timer: File,
}

impl TimerListener {
    fn new(timer_fd: RawFd) -> Self {
        Self {
            timer: unsafe { File::from_raw_fd(timer_fd) },
        }
    }
}

impl EventReceiver for TimerListener {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        let mut buffer = [0; 8];

        self.timer.read(&mut buffer[..])?;
        new_actions.add(InterestAction::PrintStats);
        new_actions.add(InterestAction::Modify(self.timer.as_raw_fd(), READ_FLAGS));
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> io::Result<()> {
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
    let content_handle = content_actor::Handle::new();
    let req_handle = request_context::Handle::new();
    let req_actor = req_handle.bind(&mut reactor, verbose, content_handle.clone())?;
    content_handle.bind(&mut reactor, verbose, req_handle)?;
    let listener = RequestListener {
        listener,
        verbose,
        req_actor,
    };
    reactor.add_interest(listener_fd, READ_FLAGS, Rc::new(RefCell::new(listener)))?;

    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    syscall!(sigemptyset(mask.as_mut_ptr()))?;
    let mut mask = unsafe { mask.assume_init() };
    syscall!(sigaddset(&mut mask, libc::SIGINT))?;
    syscall!(sigprocmask(
        libc::SIG_BLOCK,
        &mut mask,
        std::ptr::null_mut()
    ))?;
    let signal_fd = syscall!(signalfd(-1, &mask, 0))?;
    let signal_listener = SignalListener::new(signal_fd);
    reactor.add_interest(
        signal_fd,
        READ_FLAGS,
        Rc::new(RefCell::new(signal_listener)),
    )?;

    let timer_fd = syscall!(timerfd_create(libc::CLOCK_MONOTONIC, 0))?;
    let timer_spec = libc::itimerspec {
        it_value: libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        it_interval: libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };
    syscall!(timerfd_settime(
        timer_fd,
        0,
        &timer_spec,
        std::ptr::null_mut()
    ))?;
    let timer_listener = TimerListener::new(timer_fd);
    reactor.add_interest(timer_fd, READ_FLAGS, Rc::new(RefCell::new(timer_listener)))?;

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
