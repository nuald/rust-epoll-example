#[derive(Debug)]
pub struct RequestContext {
    pub stream: TcpStream,
    pub content_length: usize,
    pub buf: Vec<u8>,
    verbose: bool,
    efd: RawFd,
}

impl RequestContext {
    fn new(stream: TcpStream, verbose: bool) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(32),
            content_length: 0,
            verbose,
        }
    }

    fn parse_and_set_content_length(&mut self, data: &str) {
        let content_length_slice = "content-length: ";
        let content_length_sz = content_length_slice.len();
        if data.contains("HTTP") {
            if let Some(content_length) = data.lines().find(|l| {
                l.len() > content_length_sz
                    && l[..content_length_sz].eq_ignore_ascii_case(content_length_slice)
            }) {
                self.content_length = content_length[content_length_sz..]
                    .parse::<usize>()
                    .expect("content-length is valid");
                if self.verbose {
                    log(&format!(
                        "set content length: {} bytes",
                        self.content_length
                    ));
                }
            }
        }
    }
}

impl EventReceiver for RequestContext {
    fn on_read(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        if fd == self.efd {
            // Control message
        } else {
            // TCP request
            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf) {
                Ok(_) => {
                    if let Ok(data) = std::str::from_utf8(&buf) {
                        self.parse_and_set_content_length(data);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    return Err(e);
                }
            };
            self.buf.extend_from_slice(&buf);
            if self.buf.len() >= self.content_length {
                if self.verbose {
                    log(&format!("got all data: {} bytes", self.buf.len()));
                }
                new_actions.add(InterestAction::Modify(fd, WRITE_FLAGS));
            } else {
                new_actions.add(InterestAction::Modify(fd, READ_FLAGS));
            }
        }
        Ok(())
    }

    fn on_write(&mut self, fd: RawFd, new_actions: &mut InterestActions) -> io::Result<()> {
        match self.stream.write_all(HTTP_RESP) {
            Ok(()) => {
                if self.verbose {
                    log(&format!("answered from fd {fd}"));
                }
            },
            Err(e) =>  {
                if self.verbose {
                    log(&format!("could not answer to fd {fd}: {e}"));
                }
            }
        }
        new_actions.add(InterestAction::Remove(self.stream.as_raw_fd()));
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
    pub fn new(reactor: &mut Reactor, stream: TcpStream, verbose: bool) -> io::Result<Self> {
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
