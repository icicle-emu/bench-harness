use std::{
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
};

pub struct StatsdData {
    /// A set of buffers to hold pending data.
    buf: Vec<Vec<u8>>,

    /// Keeps track of the next free buffer to use.
    offset: usize,

    /// Keeps track of whether we have overflowed the buffers.
    is_overflow: bool,
}

impl StatsdData {
    pub fn new(capacity: usize) -> Self {
        Self { buf: vec![Vec::new(); capacity], offset: 0, is_overflow: true }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf[self.offset].clear();
        self.buf[self.offset].extend_from_slice(data);

        self.offset += 1;
        if self.offset == self.buf.len() {
            eprintln!("[agent] exceeded buffer size for statsd");
            self.is_overflow = true;
            self.offset = 0;
        }
    }

    pub fn drain_all(&mut self) -> impl Iterator<Item = &[u8]> {
        let (a, b) = if self.is_overflow {
            (&self.buf[self.offset..], &self.buf[..self.offset])
        }
        else {
            (&self.buf[..self.offset], &self.buf[..0])
        };

        self.is_overflow = false;
        self.offset = 0;

        a.iter().chain(b.iter()).map(|x| x.as_slice())
    }
}

pub fn spawn() -> Arc<Mutex<StatsdData>> {
    let data = Arc::new(Mutex::new(StatsdData::new(100)));

    let collector_data = data.clone();
    std::thread::spawn(move || {
        let addr: SocketAddr = "127.0.0.1:8125".parse().unwrap();
        loop {
            if let Err(e) = run_collector(&collector_data, &addr) {
                eprintln!("Error binding `{}`: {}", addr, e);
            }
        }
    });

    data
}

fn run_collector(data: &Mutex<StatsdData>, addr: &SocketAddr) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(addr)?;
    let mut buf = [0; 2048];

    loop {
        let n = socket.recv(&mut buf)?;
        data.lock().unwrap().push(&buf[..n]);
    }
}
