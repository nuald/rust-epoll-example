use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::rc::Rc;

use crate::EventReceiver;
use crate::Reactor;
use crate::READ_FLAGS;
use crate::InterestActions;

enum Message {
    Process {
        req: String,
        respond_to: String, // Sender<String>
    },
}

struct Actor {
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
}

impl Actor {
    fn new(ctr_queue: Rc<RefCell<VecDeque<Message>>>) -> Self {
        Self { ctr_queue }
    }

    fn handle_message(&self, msg: Message) {
        match msg {
            Message::Process { .. } => {
                // TODO: implement and notify back
            }
        }
    }
}

impl EventReceiver for Actor {
    fn on_read(&mut self, _new_actions: &mut InterestActions) -> io::Result<()> {
        for msg in self.ctr_queue.borrow_mut().drain(..) {
            self.handle_message(msg);
        }
        Ok(())
    }

    fn on_write(&mut self, _new_actions: &mut InterestActions) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct Handle {
    efd: RawFd,
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
}

impl Handle {
    #[must_use]
    pub fn new(reactor: &mut Reactor) -> io::Result<Self> {
        let ctr_queue = Rc::new(RefCell::new(VecDeque::new()));
        let actor = Actor::new(ctr_queue.clone());
        let efd = unsafe { libc::eventfd(0, libc::EFD_SEMAPHORE | libc::EFD_NONBLOCK) };
        reactor.add_interest(efd, READ_FLAGS, Rc::new(RefCell::new(actor)))?;

        Ok(Self { efd, ctr_queue })
    }

    pub fn process(&self, msg: String) {
        self.ctr_queue.borrow_mut().push_back(Message::Process {
            req: msg,
            respond_to: String::new(),
        });
        unsafe { libc::eventfd_write(self.efd, 1) };
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // epoll receives EPOLLHUP upon file close,
        // so we don't need to manually drop it
        let _ = unsafe { libc::close(self.efd) };
    }
}
