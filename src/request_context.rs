use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::raw::c_void;
use std::rc::Rc;

use crate::content_actor::Handle as ContentHandle;
use crate::content_actor::Message as ContentMessage;
use crate::{log, syscall};
use crate::reactor::EventReceiver;
use crate::reactor::InterestAction;
use crate::reactor::InterestActions;
use crate::reactor::Reactor;
use crate::reactor::READ_FLAGS;
use crate::reactor::WRITE_FLAGS;

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
    content_length: RefCell<HashMap<RawFd, usize>>,
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
            content_length: RefCell::new(HashMap::new()),
        }
    }

    fn handle_message(&self, msg: &Message, new_actions: &mut InterestActions) {
        match msg {
            Message::ContentLengthResponse {
                receiver,
                content_length,
            } => {
                self.content_length
                    .borrow_mut()
                    .insert(*receiver, *content_length);
                self.check_length(*receiver, *content_length, new_actions);
            }
        }
    }

    fn check_length(&self, fd: RawFd, length: usize, new_actions: &mut InterestActions) {
        match self.buf.get(&fd) {
            Some(buf) => {
                if buf.len() >= length {
                    if self.verbose {
                        log(&format!("got all data: {} bytes", buf.len()));
                    }
                    new_actions.add(InterestAction::Modify(fd, WRITE_FLAGS));
                } else {
                    new_actions.add(InterestAction::Modify(fd, READ_FLAGS));
                }
            }
            None => {
                if self.verbose {
                    log(&format!("unexpected fd {fd}"));
                }
            }
        }
    }
}

impl EventReceiver for RequestContext {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()> {
        if fd == self.efd {
            // Control message
            let mut value = MaybeUninit::<u64>::uninit();
            syscall!(eventfd_read(fd, value.as_mut_ptr()))?;
            for msg in self.ctr_queue.borrow_mut().drain(..) {
                self.handle_message(&msg, new_actions);
            }
            new_actions.add(InterestAction::Modify(fd, READ_FLAGS));
        } else {
            // TCP request
            let mut buf = [0u8; 4096];
            let res = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
            if res >= 0 {
                let sz = res as usize;
                self.buf
                    .entry(fd)
                    .or_insert_with(|| Vec::with_capacity(32))
                    .extend_from_slice(&buf[..sz]);
            } else if res != libc::EWOULDBLOCK as _ {
                return Err(std::io::Error::last_os_error());
            }

            match self.content_length.borrow().get(&fd) {
                None => {
                    let sz = res as usize;
                    let data = String::from_utf8_lossy(&buf[..sz]).into_owned();
                    self.content_handle
                        .enqueue(ContentMessage::ContentLengthRequest {
                            req: data,
                            sender: fd,
                        })?;
                }
                Some(length) => {
                    self.check_length(fd, *length, new_actions);
                }
            }
        }
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> std::io::Result<()> {
        let res = unsafe { libc::write(fd, HTTP_RESP.as_ptr().cast::<c_void>(), HTTP_RESP.len()) };
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
        syscall!(shutdown(fd, libc::SHUT_RDWR))?;
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
        content_handle: ContentHandle,
    ) -> std::io::Result<Rc<RefCell<RequestContext>>> {
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
