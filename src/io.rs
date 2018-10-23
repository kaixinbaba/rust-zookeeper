use consts::{ZkError, ZkState};
use proto::{ByteBuf, ConnectRequest, ConnectResponse, OpCode, ReadFrom, ReplyHeader, RequestHeader,
            WriteTo};
use watch::WatchMessage;
use zookeeper::{RawResponse, RawRequest};
use listeners::ListenerSet;

use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, BytesMut, Bytes, IntoBuf};
use mio::net::TcpStream;
use mio::*;
use mio_extras::channel::{Sender, Receiver, channel};
use mio_extras::timer::{Timer, Timeout};
use std::collections::VecDeque;
use std::io;
use std::io::Cursor;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::sync::mpsc;
use std::mem;

const ZK: Token = Token(1);
const CHANNEL: Token = Token(2);
const PING_TIMER: Token = Token(3);
const CONN_TIMER: Token = Token(4);

use try_io::{TryRead, TryWrite};

lazy_static! {
    static ref PING: ByteBuf =
    RequestHeader{xid: -2, opcode: OpCode::Ping}.to_len_prefixed_buf().unwrap();
}

struct Hosts {
    addrs: Vec<SocketAddr>,
    index: usize,
}

#[inline]
fn pollopt() -> PollOpt {
    PollOpt::edge() | PollOpt::oneshot()
}

impl Hosts {
    fn new(addrs: Vec<SocketAddr>) -> Hosts {
        Hosts {
            addrs: addrs,
            index: 0,
        }
    }

    fn get(&mut self) -> &SocketAddr {
        let addr = &self.addrs[self.index];
        if self.addrs.len() == self.index + 1 {
            self.index = 0;
        } else {
            self.index += 1;
        }
        addr
    }
}

pub struct ZkIo {
    sock: TcpStream,
    state: ZkState,
    hosts: Hosts,
    buffer: VecDeque<RawRequest>,
    inflight: VecDeque<RawRequest>,
    response: BytesMut,
    ping_timeout: Option<Timeout>,
    conn_timeout: Option<Timeout>,
    ping_timer: Timer<()>,
    conn_timer: Timer<()>,
    timeout_ms: u64,
    ping_timeout_duration: Duration,
    conn_timeout_duration: Duration,
    watch_sender: mpsc::Sender<WatchMessage>,
    conn_resp: ConnectResponse,
    zxid: i64,
    ping_sent: Instant,
    state_listeners: ListenerSet<ZkState>,
    poll: Poll,
    shutdown: bool,
    tx: Sender<RawRequest>,
    rx: Receiver<RawRequest>,
}

impl ZkIo {
    pub fn new(
        addrs: Vec<SocketAddr>,
        ping_timeout_duration: Duration,
        watch_sender: mpsc::Sender<WatchMessage>,
        state_listeners: ListenerSet<ZkState>
    ) -> ZkIo {
        trace!("ZkIo::new");
        let timeout_ms = ping_timeout_duration.as_secs() * 1000 +
            ping_timeout_duration.subsec_nanos() as u64 / 1000000;
        let (tx, rx) = channel();

        let mut zkio = ZkIo {
            sock: TcpStream::connect(&addrs[0]).unwrap(), // TODO I need a socket here, sorry.
            state: ZkState::Connecting,
            hosts: Hosts::new(addrs),
            buffer: VecDeque::new(),
            inflight: VecDeque::new(),
            // TODO server reads max up to 1MB, otherwise drops the connection,
            // size should be 1MB + tcp rcvBufsize
            response: BytesMut::with_capacity(1024 * 1024 * 2),
            ping_timeout: None,
            conn_timeout: None,
            ping_timeout_duration: ping_timeout_duration,
            conn_timeout_duration: Duration::from_secs(2),
            timeout_ms: timeout_ms,
            watch_sender: watch_sender,
            conn_resp: ConnectResponse::initial(timeout_ms),
            zxid: 0,
            ping_sent: Instant::now(),
            state_listeners: state_listeners,
            // TODO add error handling to this method in subsequent commit.
            // There's already another unwrap which needs to be addressed.
            poll: Poll::new().unwrap(),
            shutdown: false,
            ping_timer: Timer::default(),
            conn_timer: Timer::default(),
            tx: tx,
            rx: rx,
        };

        let request = zkio.connect_request();
        zkio.buffer.push_back(request);
        zkio
    }

    fn reregister(&mut self, interest: Ready) {
        self.poll
            .reregister(&self.sock, ZK, interest, pollopt())
            .expect("Failed to register ZK handle");
    }

    fn notify_state(&self, old_state: ZkState, new_state: ZkState) {
        if new_state != old_state {
            self.state_listeners.notify(&new_state);
        }
    }

    fn handle_response(&mut self) {
        loop {
            if self.response.len() <= 4 {
                return;
            }

            let len = BigEndian::read_i32(&self.response[..4]) as usize;

            trace!("Response chunk len = {} buf len is {}", len, self.response.len());

            if self.response.len() - 4 < len {
                return;
            } else {
                self.response.advance(4);
                let bytes = self.response.split_to(len);
                self.handle_chunk(bytes.freeze());

                self.response.reserve(1024 * 1024 * 2);
            }
        }
    }

    fn handle_chunk(&mut self, bytes: Bytes) {
        let len = bytes.len();
        trace!("handle_response in {:?} state [{}]", self.state, len);

        let mut data = bytes.into_buf();

        if self.state != ZkState::Connecting {
            let header = match ReplyHeader::read_from(&mut data) {
                Ok(header) => header,
                Err(e) => {
                    warn!("Failed to parse ReplyHeader {:?}", e);
                    self.inflight.pop_front();
                    return;
                }
            };
            if header.zxid > 0 {
                // Update last-seen zxid when this is a request response
                self.zxid = header.zxid;
            }
            let response = RawResponse {
                header: header,
                data: Cursor::new(data.bytes().to_vec()),
            }; // TODO COPY!
            match response.header.xid {
                -1 => {
                    trace!("handle_response Got a watch event!");
                    self.watch_sender.send(WatchMessage::Event(response)).unwrap();
                }
                -2 => {
                    trace!("Got ping response in {:?}",
                           self.ping_sent.elapsed());
                    self.inflight.pop_front();
                }
                _ => {
                    match self.inflight.pop_front() {
                        Some(request) => {
                            if request.opcode == OpCode::CloseSession {
                                let old_state = self.state;
                                self.state = ZkState::Closed;
                                self.notify_state(old_state, self.state);
                                self.shutdown = true;
                            }
                            self.send_response(request, response);
                        }
                        None => panic!("Shouldn't happen, no inflight request"),
                    }
                }
            }
        } else {
            self.inflight.pop_front(); // drop the connect request

            let conn_resp = match ConnectResponse::read_from(&mut data) {
                Ok(conn_resp) => conn_resp,
                Err(e) => {
                    panic!("Failed to parse ConnectResponse {:?}", e);
                    // self.reconnect();
                    // return
                }
            };

            let old_state = self.state;

            if conn_resp.timeout == 0 {
                info!("session {} expired", self.conn_resp.session_id);
                self.conn_resp.session_id = 0;
                self.state = ZkState::NotConnected;
            } else {
                self.conn_resp = conn_resp;
                info!("Connected: {:?}", self.conn_resp);
                self.ping_timeout_duration = Duration::from_millis(self.conn_resp.timeout / 3 * 2);

                self.state = if self.conn_resp.read_only {
                    ZkState::ConnectedReadOnly
                } else {
                    ZkState::Connected
                };
            }

            self.notify_state(old_state, self.state);
        }
    }

    fn send_response(&self, request: RawRequest, response: RawResponse) {
        match request.listener {
            Some(ref listener) => {
                trace!("send_response Opcode is {:?}", request.opcode);
                listener.send(response).unwrap();
            }
            None => info!("Nobody is interested in response {:?}", request.opcode),
        }
        if let Some(watch) = request.watch {
            self.watch_sender.send(WatchMessage::Watch(watch)).unwrap();
        }
    }

    fn clear_timeout(&mut self, atype: Token) {
        match atype {
            PING_TIMER => {
                trace!("clear_timeout: ping timeout");
                if let Some(timeout) = mem::replace(&mut self.ping_timeout , None) {
                    // Cancel existing timer
                    self.ping_timer.cancel_timeout(&timeout);
                }
            },
            CONN_TIMER => {
                trace!("clear_timeout: connection timeout");
                if let Some(timeout) = mem::replace(&mut self.conn_timeout , None) {
                    // Cancel existing timer
                    self.conn_timer.cancel_timeout(&timeout);
                }
            },
            _ => unreachable!(),
        };
    }

    fn start_timeout(&mut self, atype: Token) {
        match atype {
            PING_TIMER => {
                trace!("start_timeout: ping timeout");
                if let Some(timeout) = mem::replace(&mut self.ping_timeout, None) {
                    // Cancel existing timer
                    self.ping_timer.cancel_timeout(&timeout);
                }
                let duration = self.ping_timeout_duration.clone();
                self.ping_timeout = Some(self.ping_timer.set_timeout(duration, ()));
            },
            CONN_TIMER => {
                trace!("start_timeout: connection timeout");
                if let Some(timeout) = mem::replace(&mut self.conn_timeout, None) {
                    // Cancel existing timer
                    self.conn_timer.cancel_timeout(&timeout);
                }
                let duration = self.conn_timeout_duration.clone();
                self.conn_timeout = Some(self.conn_timer.set_timeout(duration, ()));
            },
            _ => unreachable!(),
        }
        self.reregister_timer(atype);
    }

    fn reconnect(&mut self) {
        trace!("reconnect");
        let old_state = self.state;
        self.state = ZkState::Connecting;
        self.notify_state(old_state, self.state);

        info!("Establishing Zk connection");

        // TODO only until session times out
        loop {
            self.buffer.clear();
            self.inflight.clear();
            self.response.clear(); // TODO drop all read bytes once RingBuf.clear() is merged

            // Check if the session is still alive according to our knowledge
            if self.ping_sent.elapsed().as_secs() * 1000 > self.timeout_ms {
                warn!("Zk session timeout, closing io event loop");
                self.state = ZkState::Closed;
                self.notify_state(ZkState::Connecting, self.state);
                self.shutdown = true;
                break;
            }

            self.clear_timeout(PING_TIMER);
            {
                let host = self.hosts.get();
                info!("Connecting to new server {:?}", host);
                self.poll.deregister(&self.sock).unwrap();
                self.sock = match TcpStream::connect(host) {
                    Ok(sock) => sock,
                    Err(e) => {
                        error!("Failed to connect {:?}: {:?}", host, e);
                        continue;
                    }
                };
                info!("Started connecting to {:?}", host);
            }
            self.start_timeout(CONN_TIMER);

            let request = self.connect_request();
            self.buffer.push_back(request);


            // Register the new socket
            let pollopt = PollOpt::edge() | PollOpt::oneshot();
            self.poll.register(&self.sock, ZK, Ready::all(), pollopt)
                .expect("Register ZK");

            break;
        }
    }

    fn connect_request(&self) -> RawRequest {
        let conn_req = ConnectRequest::from(&self.conn_resp, self.zxid);
        let buf = conn_req.to_len_prefixed_buf().unwrap();
        RawRequest {
            opcode: OpCode::Auth,
            data: buf,
            listener: None,
            watch: None,
        }
    }

    fn ready(&mut self, token: Token, ready: Ready) {
        trace!("event token={:?} ready={:?}", token, ready);

        match token {
            ZK => self.ready_zk(ready),
            CHANNEL => self.ready_channel(ready),
            PING_TIMER => self.ready_timer_ping(ready),
            CONN_TIMER => self.ready_timer_conn(ready),
            _ => unreachable!(),
        }
    }

    fn ready_zk(&mut self, ready: Ready) {
        self.clear_timeout(PING_TIMER);

        if ready.is_writable() {
            while let Some(mut request) = self.buffer.pop_front() {
                match self.sock.try_write_buf(&mut request.data) {
                    Ok(Some(0)) => warn!("Connection closed: write"),
                    Ok(Some(written)) => {
                        trace!("Written {:?} bytes", written);
                        if request.data.has_remaining() {
                            self.buffer.push_front(request);
                            break;
                        } else {
                            self.inflight.push_back(request);
                        }
                    }
                    Ok(None) => trace!("Spurious write"),
                    Err(e) => {
                        error!("Failed to write socket: {:?}", e);
                        self.reconnect();
                    }
                }
            }
        }

        if ready.is_readable() {
            match self.sock.try_read_buf(&mut self.response) {
                Ok(Some(0)) => warn!("Connection closed: read"),
                Ok(Some(read)) => {
                    trace!("Read {:?} bytes", read);
                    self.handle_response();
                }
                Ok(None) => trace!("Spurious read"),
                Err(e) => {
                    error!("Failed to read socket: {:?}", e);
                    self.reconnect();
                }
            }

        }

        if (ready.is_hup()) && (self.state != ZkState::Closed) {
            // If we were connected
            // fn send_watched_event(keeper_state: KeeperState) {
            //     match sender.send(WatchedEvent{event_type: WatchedEventType::None,
            //                                    keeper_state: keeper_state,
            //                                    path: None}) {
            //         Ok(()) => (),
            //         Err(e) => panic!("Reader/Writer: Event died {}", e)
            //     }
            // }
            let old_state = self.state;
            self.state = ZkState::NotConnected;
            self.notify_state(old_state, self.state);

            info!("Reconnect due to HUP");
            self.reconnect();
        }

        if self.is_idle() {
            self.start_timeout(PING_TIMER);
        }

        // Not sure that we need to write, but we always need to read, because of watches
        // If the output buffer has no content, we don't need to write again
        let mut interest = Ready::all();
        if self.buffer.is_empty() {
            interest.remove(Ready::writable());
        }

        // This tick is done, subscribe to a forthcoming one
        self.reregister(interest);
    }

    fn is_idle(&self) -> bool {
        self.inflight.is_empty() && self.buffer.is_empty()
    }

    fn ready_channel(&mut self, _: Ready) {
        while let Ok(request) = self.rx.try_recv() {
            trace!("ready_channel {:?}", request.opcode);

            match self.state {
                ZkState::Closed => {
                    // If zk is unavailable, respond with a ConnectionLoss error.
                    let header = ReplyHeader {
                        xid: 0,
                        zxid: 0,
                        err: ZkError::ConnectionLoss as i32,
                    };
                    let response = RawResponse {
                        header: header,
                        data: ByteBuf::new(vec![]),
                    };
                    self.send_response(request, response);
                },
                _ => {
                    // Otherwise, queue request for processing.
                    if self.buffer.is_empty() {
                        self.reregister(Ready::all());
                    }
                    self.buffer.push_back(request);
                },
            }
        }

        self.poll.reregister(&self.rx, CHANNEL, Ready::readable(), pollopt())
            .expect("Reregister CHANNEL");
    }

    fn ready_timer_ping(&mut self, _: Ready) {
        trace!("ready_timer_ping; thread={:?}", ::std::thread::current().id());

        loop {
            match self.ping_timer.poll() {
                Some(_) => {
                    self.clear_timeout(PING_TIMER);
                    if self.inflight.is_empty() {
                        // No inflight request indicates an idle connection. Send a ping.
                        trace!("Pinging {:?}", self.sock.peer_addr().unwrap());
                        self.tx.send(RawRequest {
                            opcode: OpCode::Ping,
                            data: PING.clone(),
                            listener: None,
                            watch: None,
                        }).unwrap();
                        self.ping_sent = Instant::now();
                    }
                },
                None => {
                    if self.ping_timeout.is_some() {
                        // Reregister on a spurious wakeup
                        self.reregister_timer(PING_TIMER)
                    }

                    break;
                }
            }
        }
    }

    fn ready_timer_conn(&mut self, _: Ready) {
        trace!("ready_timer_conn; thread={:?}", ::std::thread::current().id());

        loop {
            match self.conn_timer.poll() {
                Some(_) => {
                    self.clear_timeout(CONN_TIMER);
                    if self.state == ZkState::Connecting {
                        info!("Reconnect due to connection timeout");
                        self.reconnect();
                    }
                },
                None => {
                    if self.conn_timeout.is_some() {
                        // Reregister on a spurious wakeup
                        self.reregister_timer(CONN_TIMER)
                    }

                    break;
                }
            }
        }
    }

    fn reregister_timer(&self, atype: Token) {
        trace!("Reregister timer={:?}", atype);
        let timer = match atype {
            PING_TIMER => &self.ping_timer,
            CONN_TIMER => &self.conn_timer,
            _ => unreachable!(),
        };
        self.poll.reregister(timer, atype, Ready::readable(), pollopt()).expect("Register TIMER");
    }

    pub fn sender(&self) -> Sender<RawRequest> {
        self.tx.clone()
    }

    pub fn run(mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(128);

        // Register Initial Interest
        self.poll.register(&self.sock, ZK, Ready::all(), pollopt())
            .expect("Register ZK");
        self.poll.register(&self.ping_timer, PING_TIMER, Ready::readable(), pollopt())
            .expect("Register PING_TIMER");
        self.poll.register(&self.conn_timer, CONN_TIMER, Ready::readable(), pollopt())
            .expect("Register CONN_TIMER");
        self.poll.register(&self.rx, CHANNEL, Ready::readable(), pollopt())
            .expect("Register CHANNEL");

        loop {
            // Handle loop shutdown
            if self.shutdown {
                break;
            }

            // Wait for events
            self.poll.poll(&mut events, None)?;

            // Process events
            for event in &events {
                self.ready(event.token(), event.readiness());
            }
        }

        Ok(())
    }
}
