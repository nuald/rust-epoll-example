use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::os::raw::c_void;
use std::rc::Rc;

use crate::content_actor::Handle as ContentHandle;
use crate::content_actor::Message as ContentMessage;
use crate::log;
use crate::EventReceiver;
use crate::InterestAction;
use crate::InterestActions;
use crate::Reactor;
use crate::READ_FLAGS;
use crate::WRITE_FLAGS;

const HTTP_RESP: &[u8] = br"HTTP/1.1 200 OK
content-type: text/html
content-length: 5

Hello";

pub struct RequestContext {
    buf: HashMap<RawFd, Vec<u8>>,
    verbose: bool,
    efd: RawFd,
    ctr_queue: Rc<RefCell<VecDeque<Message>>>,
    content_handle: ContentHandle,
}

pub enum Message {
    ContentLengthResponse {
        receiver: RawFd,
        content_length: usize,
    },
}

impl RequestContext {
    fn new(
        ctr_queue: Rc<RefCell<VecDeque<Message>>>,
        efd: RawFd,
        verbose: bool,
        content_handle: ContentHandle,
    ) -> Self {
        Self {
            buf: HashMap::new(),
            verbose,
            ctr_queue,
            efd,
            content_handle,
        }
    }

    fn handle_message(&self, msg: Message, new_actions: &mut InterestActions) {
        match msg {
            Message::ContentLengthResponse {
                receiver,
                content_length,
            } => match self.buf.get(&receiver) {
                Some(buf) => {
                    if buf.len() >= content_length {
                        if self.verbose {
                            log(&format!("got all data: {} bytes", buf.len()));
                        }
                        new_actions.add(InterestAction::Modify(receiver, WRITE_FLAGS));
                    } else {
                        new_actions.add(InterestAction::Modify(receiver, READ_FLAGS));
                    }
                }
                None => {
                    if self.verbose {
                        log(&format!("unexpected fd {receiver}"));
                    }
                }
            },
        }
    }
}

impl EventReceiver for RequestContext {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        if fd == self.efd {
            // Control message
            for msg in self.ctr_queue.borrow_mut().drain(..) {
                self.handle_message(msg, new_actions);
            }
        } else {
            // TCP request
            let mut buf = [0u8; 4096];
            let res = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if res > 0 {
                let sz = res as usize;
                let data = String::from_utf8_lossy(&buf[..sz]).into_owned();
                self.content_handle
                    .enqueue(ContentMessage::ContentLengthRequest {
                        req: data,
                        sender: fd,
                    });
                self.buf
                    .entry(fd)
                    .or_insert_with(|| Vec::with_capacity(32))
                    .extend_from_slice(&buf[..sz]);
            } else if res != libc::EWOULDBLOCK as _ {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        let res = unsafe { libc::write(fd, HTTP_RESP.as_ptr() as *const c_void, HTTP_RESP.len()) };
        if res > 0 {
            if self.verbose {
                log(&format!("answered from fd {fd}"));
            }
        } else {
            let e = std::io::Error::last_os_error();
            if self.verbose {
                log(&format!("could not answer to fd {fd}: {e}"));
            }
        }
        new_actions.add(InterestAction::Remove(fd));
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
        content_handle: ContentHandle,
    ) -> io::Result<Rc<RefCell<RequestContext>>> {
        let actor = Rc::new(RefCell::new(RequestContext::new(
            self.ctr_queue.clone(),
            self.efd,
            verbose,
            content_handle,
        )));
        reactor.add_interest(self.efd, READ_FLAGS, actor.clone())?;
        Ok(actor)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // epoll receives EPOLLHUP upon file close,
        // so we don't need to manually drop it
        let _ = unsafe { libc::close(self.efd) };
    }
}
