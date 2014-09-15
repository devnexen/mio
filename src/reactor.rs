use error::{MioResult, MioError};
use handler::{Handler, Token};
use io::*;
use os;
use poll::Poll;

/// A lightweight IO reactor.
///
/// An internal lookup structure is used to associate tokens with io
/// descriptors as well as track whether a socket is a listener or not.

#[deriving(Clone, Show)]
pub struct ReactorConfig;

pub struct Reactor<T> {
    poll: Poll<T>,
    run: bool
}

pub type ReactorResult<H> = Result<H, ReactorError<H>>;

pub struct ReactorError<H> {
    pub handler: H,
    pub error: MioError
}

impl<H> ReactorError<H> {
    fn new(handler: H, error: MioError) -> ReactorError<H> {
        ReactorError {
            handler: handler,
            error: error
        }
    }
}

impl<T: Token> Reactor<T> {
    /// Initializes a new reactor. The reactor will not be running yet.
    pub fn new() -> MioResult<Reactor<T>> {
        Ok(Reactor {
            poll: try!(Poll::new()),
            run: true
        })
    }

    /// Tells the reactor to exit after it is done handling all events in the
    /// current iteration.
    pub fn shutdown(&mut self) {
        self.run = false;
    }

    /// Tells the reactor to exit immidiately. All pending events will be dropped.
    pub fn shutdown_now(&mut self) {
        unimplemented!()
    }

    /// Registers an IO handle with the reactor.
    pub fn register<H: IoHandle>(&mut self, io: &H, token: T) -> MioResult<()> {
        self.poll.register(io, token)
    }

    /// Connects the socket to the specified address. When the operation
    /// completes, the handler will be notified with the supplied token.
    ///
    /// The goal of this method is to ensure that the reactor will always
    /// notify about the connection, even if the connection happens
    /// immediately. Otherwise, every consumer of the reactor would have
    /// to worry about possibly-immediate connection.
    pub fn connect<S: Socket>(&mut self, io: &S,
                              addr: &SockAddr, token: T) -> MioResult<()> {

        debug!("socket connect; addr={}", addr);

        // Attempt establishing the context. This may not complete immediately.
        if try!(os::connect(io.desc(), addr)) {
            // On some OSs, connecting to localhost succeeds immediately. In
            // this case, queue the writable callback for execution during the
            // next reactor tick.
            debug!("socket connected immediately; addr={}", addr);
        }

        // Register interest with socket on the reactor
        try!(self.poll.register(io, token));

        Ok(())
    }

    pub fn listen<S, A: IoHandle + IoAcceptor<S>>(&mut self, io: &A, backlog: uint,
                                                  token: T) -> MioResult<()> {

        debug!("socket listen");

        // Start listening
        try!(os::listen(io.desc(), backlog));

        // Wait for connections
        try!(self.poll.register(io, token));

        Ok(())
    }

    /// Keep spinning the reactor indefinitely, and notify the handler whenever
    /// any of the registered handles are ready.
    pub fn run<H: Handler<T>>(&mut self, mut handler: H) -> ReactorResult<H> {
        self.run = true;

        while self.run {
            // Execute ticks as long as the reactor is running
            match self.tick(&mut handler) {
                Err(e) => return Err(ReactorError::new(handler, e)),
                _ => {}
            }
        }

        Ok(handler)
    }

    /// Spin the reactor once, with a timeout of one second, and notify the
    /// handler if any of the registered handles become ready during that
    /// time.
    pub fn run_once<H: Handler<T>>(&mut self, mut handler: H) -> ReactorResult<H> {
        // Execute a single tick
        match self.tick(&mut handler) {
            Err(e) => return Err(ReactorError::new(handler, e)),
            _ => {}
        }

        Ok(handler)
    }

    // Executes a single run of the reactor loop
    fn tick<H: Handler<T>>(&mut self, handler: &mut H) -> MioResult<()> {
        debug!("reactor tick");

        // Check the registered IO handles for any new events. Each poll
        // is for one second, so a shutdown request can last as long as
        // one second before it takes effect.
        try!(self.io_poll(handler));

        Ok(())
    }

    /// Poll the reactor for one second, calling the handler if any
    /// of the registered handles are ready.
    fn io_poll<H: Handler<T>>(&mut self, handler: &mut H) -> MioResult<()> {
        let cnt = try!(self.poll.poll(1000));

        let mut i = 0u;

        // Iterate over the notifications. Each event provides the token
        // it was registered with (which usually represents, at least, the
        // handle that the event is about) as well as information about
        // what kind of event occurred (readable, writable, signal, etc.)
        while i < cnt {
            let evt = self.poll.event(i);
            let tok = evt.token();

            debug!("event={}", evt);

            if evt.is_readable() {
                handler.readable(self, tok);
            }

            if evt.is_writable() {
                handler.writable(self, tok);
            }

            if evt.is_error() {
                println!(" + ERROR");
            }

            i += 1;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std;
    use std::sync::Arc;
    use std::sync::atomics::{AtomicInt, SeqCst};

    use super::Reactor;
    use buf::{SliceBuf, MutSliceBuf};
    use io::{IoWriter, IoReader};
    use Handler;
    use os;

    struct Funtimes {
        readable: Arc<AtomicInt>,
        writable: Arc<AtomicInt>
    }

    impl Funtimes {
        fn new(readable: Arc<AtomicInt>, writable: Arc<AtomicInt>) -> Funtimes {
            Funtimes { readable: readable, writable: writable }
        }
    }

    impl Handler<u64> for Funtimes {
        fn readable(&mut self, _reactor: &mut Reactor<u64>, token: u64) {
            (*self.readable).fetch_add(1, SeqCst);
            assert_eq!(token, 10u64);
        }
    }

    #[test]
    fn test_readable() {
        let mut reactor = Reactor::<u64>::new().ok().expect("Couldn't make reactor");
        let pipe = unsafe { std::os::pipe() }.ok().expect("Couldn't create pipe");
        let mut reader = os::IoDesc { fd: pipe.reader };
        let mut writer = os::IoDesc { fd: pipe.writer };

        let mut buf = SliceBuf::wrap("hello".as_bytes());

        let read_count = Arc::new(AtomicInt::new(0));
        let write_count = Arc::new(AtomicInt::new(0));

        writer.write(&mut buf).unwrap();

        reactor.register(&reader, 10u64).unwrap();
        let _ = reactor.run_once(Funtimes::new(read_count.clone(), write_count.clone()));

        assert_eq!((*read_count).load(SeqCst), 1);

        let mut write_vec = vec![0u8, 0u8, 0u8, 0u8, 0u8];

        {
            let mut write_into = MutSliceBuf::wrap(write_vec.as_mut_slice());
            reader.read(&mut write_into).ok().expect("Couldn't read");
        }

        assert_eq!(String::from_utf8(write_vec).ok().expect("Invalid UTF-8").as_slice(), "hello");
    }
}
