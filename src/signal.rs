use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::raw::c_void;

use crate::reactor::State;
use crate::syscall;
use crate::EventReceiver;
use crate::InterestAction;
use crate::InterestActions;

pub struct Listener {
    pub fd: RawFd,
}

impl Listener {
    pub(crate) fn new() -> std::io::Result<Self> {
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        syscall!(sigemptyset(mask.as_mut_ptr()))?;
        let mut mask = unsafe { mask.assume_init() };
        syscall!(sigaddset(&mut mask, libc::SIGINT))?;
        syscall!(sigprocmask(
            libc::SIG_BLOCK,
            &mut mask,
            std::ptr::null_mut()
        ))?;
        let fd = syscall!(signalfd(-1, &mask, 0))?;

        Ok(Self { fd })
    }
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
        let mut siginfo = MaybeUninit::<libc::signalfd_siginfo>::uninit();
        let siginfo_size = std::mem::size_of::<libc::signalfd_siginfo>();
        syscall!(read(
            fd,
            siginfo.as_mut_ptr().cast::<c_void>(),
            siginfo_size
        ))?;

        new_actions.add(InterestAction::Exit);
        Ok(())
    }
}
