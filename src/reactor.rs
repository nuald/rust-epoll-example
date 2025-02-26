use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::fd::RawFd;
use std::rc::Rc;

use crate::{log, syscall};

pub trait EventReceiver {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()>;
    fn on_write(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()>;
}

pub const READ_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLIN;
pub const WRITE_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLOUT;

pub enum InterestAction {
    Add(RawFd, i32, Rc<RefCell<dyn EventReceiver>>),
    Modify(RawFd, i32),
    Remove(RawFd),
    Exit,
    PrintStats,
}

pub struct InterestActions {
    actions: VecDeque<InterestAction>,
}

impl InterestActions {
    fn new() -> Self {
        Self {
            actions: VecDeque::new(),
        }
    }

    pub fn add(&mut self, action: InterestAction) {
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
    pub(crate) fn new() -> std::io::Result<Self> {
        let epoll_fd = syscall!(epoll_create1(0))?;
        if let Ok(flags) = syscall!(fcntl(epoll_fd, libc::F_GETFD)) {
            let _ = syscall!(fcntl(epoll_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC));
        }
        Ok(Self {
            epoll_fd,
            receivers: HashMap::new(),
        })
    }

    pub(crate) fn add_interest(
        &mut self,
        fd: RawFd,
        flags: i32,
        receiver: Rc<RefCell<dyn EventReceiver>>,
    ) -> std::io::Result<()> {
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

    fn modify_interest(&self, fd: RawFd, flags: i32) -> std::io::Result<()> {
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

    fn remove_interest(&mut self, fd: RawFd) -> std::io::Result<()> {
        syscall!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_DEL,
            fd,
            std::ptr::null_mut()
        ))?;
        self.receivers.remove(&fd);
        let _ = unsafe { libc::close(fd) };
        Ok(())
    }

    fn apply(&mut self, actions: InterestActions) -> std::io::Result<bool> {
        let mut exit = false;
        for action in actions {
            match action {
                InterestAction::Add(fd, flags, receiver) => {
                    self.add_interest(fd, flags, receiver)?;
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

    pub(crate) fn run(&mut self, verbose: bool) -> std::io::Result<()> {
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
