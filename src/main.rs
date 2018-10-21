use std::alloc::System;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use byteorder::{WriteBytesExt, BE};
use pnet_packet::icmp::echo_request;
use pnet_packet::icmp::{Icmp, IcmpTypes};
use structopt::StructOpt;
use tokio::prelude::*;
use tokio::runtime::current_thread::Runtime;
use tokio::timer::Delay;
use tokio_icmp::Socket as ICMPSocket;

#[global_allocator]
static A: System = System;

#[derive(Debug)]
struct PingInfo {
    host: Ipv4Addr, // sender
    sequence: u16,  // icmp sequence number
    time: Duration, // round-trip time
}

struct State {
    last_packet_time: Instant,
    sequence: u16,
    identifier: u16,
    packets_sent: usize,
    packets_received: usize,
}

struct Ping {
    socket: ICMPSocket,
    state: State,
}

impl Ping {
    pub fn new(id: u16) -> io::Result<Ping> {
        let socket = ICMPSocket::new()?;

        Ok(Ping {
            socket,

            state: State {
                last_packet_time: Instant::now(),
                sequence: 0,
                identifier: id,
                packets_sent: 0,
                packets_received: 0,
            },
        })
    }

    fn make_echo_request(&mut self) -> Icmp {
        let payload = {
            let bytes = Vec::with_capacity(4);
            let mut cursor = io::Cursor::new(bytes);

            cursor.write_u16::<BE>(self.state.identifier).unwrap();
            cursor.write_u16::<BE>(self.state.sequence).unwrap();
            self.state.sequence += 1;

            cursor.into_inner()
        };

        Icmp {
            icmp_type: IcmpTypes::EchoRequest,
            icmp_code: echo_request::IcmpCodes::NoCode,
            checksum: 0,
            payload,
        }
    }

    // send icmp echo request to |host|
    fn send_to(mut self, host: Ipv4Addr) -> impl Future<Item = Self, Error = io::Error> {
        let packet = self.make_echo_request();
        let sockaddr = SocketAddrV4::new(host, 0);
        let socket = self.socket;
        let mut state = self.state;

        socket
            .send_to(packet, sockaddr.into())
            .map(move |(socket, _)| {
                state.last_packet_time = Instant::now();
                state.packets_sent += 1;
                Ping { socket, state }
            })
    }

    // wait for icmp echo reply from |host|
    // FIXME: ignores sequence number and identification
    fn recv_from(self, host: Ipv4Addr) -> impl Future<Item = (Self, Icmp), Error = io::Error> {
        let state = self.state;
        let socket = self.socket;

        future::loop_fn((socket, state, host), |(socket, state, host)| {
            socket.recv_from().map(move |(socket, addr, packet)| {
                let ip = *addr.as_inet().unwrap().ip();

                if ip == host {
                    let mut ping = Ping { socket, state };
                    ping.state.packets_received += 1;

                    future::Loop::Break((ping, packet))
                } else {
                    future::Loop::Continue((socket, state, host))
                }
            })
        })
    }

    // TODO: add timeout
    pub fn pings(self, host: Ipv4Addr) -> impl Stream<Item = PingInfo, Error = io::Error> {
        stream::unfold(self, move |ping| {
            let host = host.clone();

            let deadline = Instant::now() + Duration::from_secs(1);
            let fut = Delay::new(deadline)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                .and_then(move |_| ping.send_to(host.clone()))
                .and_then(move |ping| ping.recv_from(host.clone()))
                .map(move |(ping, _icmp)| {
                    let info = PingInfo {
                        host,
                        sequence: ping.state.sequence - 1,
                        time: Instant::now() - ping.state.last_packet_time,
                    };

                    (info, ping)
                });

            Some(fut)
        })
    }
}

#[derive(StructOpt, Debug)]
struct Opts {
    host: Ipv4Addr,
}

fn run() -> io::Result<()> {
    let opts = Opts::from_args();

    let id = std::process::id();
    let ping = Ping::new(id as u16)?;

    let signals = tokio_signal::ctrl_c().flatten_stream().map(|_| None);
    let process_pings =
        ping.pings(opts.host)
            .map(Some)
            .select(signals)
            .for_each(|event| match event {
                Some(info) => {
                    println!(
                        "Received echo reply #{:3} from {} round-trip time is {:?}",
                        info.sequence, info.host, info.time
                    );
                    future::ok(())
                }
                None => future::err(io::Error::new(io::ErrorKind::Other, "ctrl+c")),
            });

    let mut runtime = Runtime::new()?;
    runtime.block_on(process_pings)?;

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("An error occured: {}", e);
    }
}
