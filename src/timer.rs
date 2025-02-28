use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::raw::c_void;

use crate::syscall;
use crate::reactor::{State, EventReceiver, InterestAction, InterestActions, READ};

pub struct Listener {
    fd: RawFd,
}

impl Listener {
    pub(crate) fn new() -> std::io::Result<Self> {
        let fd = syscall!(timerfd_create(libc::CLOCK_MONOTONIC, 0))?;
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
        syscall!(timerfd_settime(fd, 0, &timer_spec, std::ptr::null_mut()))?;
        Ok(Self { fd })
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
        let mut expire_num = MaybeUninit::<u64>::uninit();
        let expire_num_size = size_of::<u64>();
        syscall!(read(
            fd,
            expire_num.as_mut_ptr().cast::<c_void>(),
            expire_num_size
        ))?;

        new_actions.add(InterestAction::PrintStats);
        new_actions.add(InterestAction::Modify(self.fd, READ));
        Ok(())
    }
}
