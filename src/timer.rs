use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::raw::c_void;

use crate::syscall;
use crate::EventReceiver;
use crate::InterestAction;
use crate::InterestActions;
use crate::READ_FLAGS;

pub struct Listener {
    pub fd: RawFd,
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
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.fd) };
    }
}

impl EventReceiver for Listener {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()> {
        let mut expire_num = MaybeUninit::<u64>::uninit();
        let expire_num_size = std::mem::size_of::<u64>();
        syscall!(read(
            fd,
            expire_num.as_mut_ptr().cast::<c_void>(),
            expire_num_size
        ))?;

        new_actions.add(InterestAction::PrintStats);
        new_actions.add(InterestAction::Modify(self.fd, READ_FLAGS));
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> std::io::Result<()> {
        Ok(())
    }
}
