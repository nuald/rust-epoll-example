use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::rc::Rc;

use crate::{log, syscall};
use crate::reactor::{State, EventReceiver, InterestAction, InterestActions, Reactor, READ};

use crate::request_context::Handle as ReqHandle;
use crate::request_context::Message as ReqMessage;

pub enum Message {
    ContentLengthRequest { req: String, sender: RawFd },
}

struct Actor {
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
    verbose: bool,
    req_handle: ReqHandle,
}

impl Actor {
    fn new(
        ctr_queue: Rc<RefCell<VecDeque<Message>>>,
        verbose: bool,
        req_handle: ReqHandle,
    ) -> Self {
        Self {
            ctr_queue,
            verbose,
            req_handle,
        }
    }

    fn parse_and_set_content_length(&self, data: &str) -> usize {
        let mut result = 0;
        let content_length_slice = "content-length: ";
        let content_length_sz = content_length_slice.len();
        if data.contains("HTTP") {
            if let Some(content_length) = data.lines().find(|l| {
                l.len() > content_length_sz
                    && l[..content_length_sz].eq_ignore_ascii_case(content_length_slice)
            }) {
                result = content_length[content_length_sz..]
                    .parse::<usize>()
                    .expect("content-length is valid");
                if self.verbose {
                    log(&format!("set content length: {result} bytes"));
                }
            }
        }
        result
    }

    fn handle_message(&self, msg: Message) -> std::io::Result<()> {
        match msg {
            Message::ContentLengthRequest { req, sender } => {
                let content_length = self.parse_and_set_content_length(&req);
                self.req_handle.enqueue(ReqMessage::ContentLengthResponse {
                    receiver: sender,
                    content_length,
                })
            }
        }
    }
}

impl EventReceiver for Actor {
    fn on_ready(
        &mut self,
        ready_to: State,
        fd: RawFd,
        new_actions: &mut InterestActions,
    ) -> std::io::Result<()> {
        debug_assert!(ready_to.read());
        let mut value = MaybeUninit::<u64>::uninit();
        syscall!(eventfd_read(fd, value.as_mut_ptr()))?;
        for msg in self.ctr_queue.borrow_mut().drain(..) {
            self.handle_message(msg)?;
        }
        new_actions.add(InterestAction::Modify(fd, READ));
        Ok(())
    }
}

#[derive(Clone)]
pub struct Handle {
    efd: RawFd,
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
}

impl Handle {
    pub(crate) fn new() -> std::io::Result<Self> {
        let ctr_queue = Rc::new(RefCell::new(VecDeque::new()));
        let efd = syscall!(eventfd(0, libc::EFD_SEMAPHORE | libc::EFD_NONBLOCK))?;

        Ok(Self { efd, ctr_queue })
    }

    pub(crate) fn enqueue(&self, msg: Message) -> std::io::Result<()> {
        self.ctr_queue.borrow_mut().push_back(msg);
        syscall!(eventfd_write(self.efd, 1))?;
        Ok(())
    }

    pub(crate) fn bind(
        &self,
        reactor: &mut Reactor,
        verbose: bool,
        req_handle: ReqHandle,
    ) -> std::io::Result<()> {
        let actor = Actor::new(self.ctr_queue.clone(), verbose, req_handle);
        reactor.add_interest(self.efd, READ, Rc::new(RefCell::new(actor)))?;
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // epoll receives EPOLLHUP upon file close,
        // so we don't need to manually drop it
        let _ = unsafe { libc::close(self.efd) };
    }
}
