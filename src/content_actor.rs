use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::rc::Rc;

use crate::log;
use crate::EventReceiver;
use crate::InterestActions;
use crate::Reactor;
use crate::READ_FLAGS;

use crate::request_context::Handle as ReqHandle;
use crate::request_context::Message as ReqMessage;

pub enum Message {
    ContentLengthRequest { req: String, sender: RawFd },
}

struct Actor {
    verbose: bool,
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
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
                    log(&format!("set content length: {} bytes", result));
                }
            }
        }
        result
    }

    fn handle_message(&self, msg: Message) {
        match msg {
            Message::ContentLengthRequest { req, sender } => {
                let content_length = self.parse_and_set_content_length(&req);
                self.req_handle.enqueue(ReqMessage::ContentLengthResponse {
                    receiver: sender,
                    content_length,
                });
            }
        }
    }
}

impl EventReceiver for Actor {
    fn on_read(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> io::Result<()> {
        for msg in self.ctr_queue.borrow_mut().drain(..) {
            self.handle_message(msg);
        }
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, _new_actions: &mut InterestActions) -> io::Result<()> {
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
    pub fn new() -> Self {
        let ctr_queue = Rc::new(RefCell::new(VecDeque::new()));
        let efd = unsafe { libc::eventfd(0, libc::EFD_SEMAPHORE | libc::EFD_NONBLOCK) };

        Self { efd, ctr_queue }
    }

    pub fn enqueue(&self, msg: Message) {
        self.ctr_queue.borrow_mut().push_back(msg);
        unsafe { libc::eventfd_write(self.efd, 1) };
    }

    pub fn bind(
        &self,
        reactor: &mut Reactor,
        verbose: bool,
        req_handle: ReqHandle,
    ) -> io::Result<()> {
        let actor = Actor::new(self.ctr_queue.clone(), verbose, req_handle);
        reactor.add_interest(self.efd, READ_FLAGS, Rc::new(RefCell::new(actor)))?;
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
