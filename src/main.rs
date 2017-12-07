extern crate nix;
extern crate net2;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_timer;
extern crate env_logger;
extern crate ini;
#[macro_use]
extern crate clap;
#[macro_use]
extern crate log;
extern crate moproxy;
use std::cmp;
use std::env;
use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::net::{SocketAddr, SocketAddrV4};
use std::io::{self, ErrorKind};
use std::os::unix::io::{RawFd, AsRawFd};
use ini::Ini;
use futures::{future, stream, Future, Stream};
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::{Core, Handle};
use tokio_timer::Timer;
use nix::sys::socket;
use log::LogLevelFilter;
use env_logger::{LogBuilder, LogTarget};
use moproxy::monitor::{self, ServerList};
use moproxy::proxy::{self, ProxyServer};
use moproxy::proxy::ProxyProto::{Socks5, Http};
use moproxy::web;


fn main() {
    let yaml = load_yaml!("cli.yml");
    let args = clap::App::from_yaml(yaml).get_matches();

    let mut logger = LogBuilder::new();
    if let Ok(env_log) = env::var("RUST_LOG") {
        logger.parse(&env_log);
    }
    let log_level = args.value_of("log-level")
        .unwrap_or("info").parse()
        .expect("unknown log level");
    logger.filter(None, log_level)
        .filter(Some("tokio_core"), LogLevelFilter::Warn)
        .filter(Some("ini"), LogLevelFilter::Warn)
        .target(LogTarget::Stdout)
        .format(|r| format!("[{}] {}", r.level(), r.args()))
        .init()
        .expect("cannot set logger");

    let host = args.value_of("host")
        .expect("missing host").parse()
        .expect("invalid address");
    let port = args.value_of("port")
        .expect("missing port number").parse()
        .expect("invalid port number");
    let addr = SocketAddr::new(host, port);
    let probe = args.value_of("probe-secs")
        .expect("missing probe secs").parse()
        .expect("not a vaild probe secs");

    let servers = parse_servers(&args);
    if servers.len() == 0 {
        panic!("missing server list");
    }
    info!("total {} server(s) added", servers.len());
    let servers = Arc::new(ServerList::new(servers));

    if let Some(addr) = args.value_of("web-bind") {
        let servers = servers.clone();
        let addr = addr.parse()
            .expect("not a valid address");
        thread::spawn(move || web::run_server(addr, servers));
    }

    let mut lp = Core::new().expect("fail to create event loop");
    let handle = lp.handle();

    let listener = TcpListener::bind(&addr, &handle)
        .expect("cannot bind to port");
    info!("listen on {}", addr);
    let mon = monitor::monitoring_servers(
        servers.clone(), probe, lp.handle());
    handle.spawn(mon);
    let server = listener.incoming().for_each(move |(client, addr)| {
        debug!("incoming {}", addr);
        let list = servers.clone();
        let conn = connect_server(client, list.clone(), handle.clone());
        let serv = conn.and_then(|(client, proxy, (dest, idx))| {
            let timeout = Some(Duration::from_secs(180));
            if let Err(e) = client.set_keepalive(timeout)
                    .and(proxy.set_keepalive(timeout)) {
                warn!("fail to set keepalive: {}", e);
            }
            list.update_stats_conn_open(idx);
            proxy::piping(client, proxy).then(move |result| match result {
                Ok((tx, rx)) => {
                    list.update_stats_conn_close(idx, tx, rx);
                    debug!("tx {}, rx {} bytes ({} => {})",
                        tx, rx, list.servers[idx].tag, dest);
                    Ok(())
                },
                Err(e) => {
                    list.update_stats_conn_close(idx, 0, 0);
                    warn!("{} (=> {}) piping error: {}",
                        list.servers[idx].tag, dest, e);
                    Err(())
                },
            })
        });
        handle.spawn(serv);
        Ok(())
    });
    lp.run(server).expect("error on event loop");
}

fn parse_servers(args: &clap::ArgMatches) -> Vec<ProxyServer> {
    let default_test_ip = args.value_of("test-ip")
        .expect("missing test-ip").parse()
        .expect("not a valid ip address");
    let mut servers: Vec<ProxyServer> = vec![];
    if let Some(s) = args.values_of("socks5-servers") {
        for s in s.map(parse_server) {
            servers.push(ProxyServer::new(
                    s, Socks5, default_test_ip, None, None));
        }
    }
    if let Some(s) = args.values_of("http-servers") {
        for s in s.map(parse_server) {
            servers.push(ProxyServer::new(
                    s, Http, default_test_ip, None, None));
        }
    }
    if let Some(path) = args.value_of("server-list") {
        let ini = Ini::load_from_file(path)
            .expect("cannot read server list file");
        for (tag, props) in ini.iter() {
            let tag = if let Some(s) = props.get("tag") {
                Some(s.as_str())
            } else if let Some(ref s) = *tag {
                Some(s.as_str())
            } else {
                None
            };
            let addr: SocketAddr = props.get("address")
                .expect("address not specified").parse()
                .expect("not a valid socket address");
            let proto = props.get("protocol")
                .expect("protocol not specified").parse()
                .expect("unknown proxy protocol");
            let base = props.get("score base").map(|i| i.parse()
                .expect("score base not a integer"));
            let test_ip = props.get("test ip").map(|i| i.parse()
                .expect("not a valid ip address"))
                .unwrap_or(default_test_ip);
            servers.push(ProxyServer::new(addr, proto, test_ip, tag, base));
        }
    }
    servers
}

fn parse_server(addr: &str) -> SocketAddr {
    if addr.contains(":") {
        addr.parse()
    } else {
        format!("127.0.0.1:{}", addr).parse()
    }.expect("not a valid server address")
}

fn connect_server(client: TcpStream, list: Arc<ServerList>, handle: Handle)
        -> Box<Future<Item=(TcpStream, TcpStream,
                           (SocketAddr, usize)), Error=()>> {
    let src_dst = future::result(client.peer_addr())
        .join(future::result(get_original_dest(client.as_raw_fd())))
        .map_err(|err| warn!("fail to get original destination: {}", err));
    // TODO: reuse timer?
    let timer = Timer::default();
    let infos = list.get_infos().clone();
    let try_connect_all = src_dst.and_then(move |(src, dest)| {
        stream::iter_ok(infos).for_each(move |info| {
            let server = list.servers[info.idx].clone();
            let conn = server.connect(dest, &handle);
            let wait = if let Some(delay) = info.delay {
                cmp::max(Duration::from_secs(3), delay * 2)
            } else {
                Duration::from_secs(3)
            };
            // Standard proxy server need more time (e.g. DNS resolving)
            timer.timeout(conn, wait).then(move |result| match result {
                Ok(conn) => {
                    info!("{} => {} via {}", src, dest, server.tag);
                    Err((conn, (dest, info.idx)))
                },
                Err(err) => {
                    warn!("fail to connect {}: {}", server.tag, err);
                    Ok(())
                }
            })
        }).then(|result| match result {
            Err(args) => Ok(args),
            Ok(_) => {
                warn!("all proxy server down");
                Err(())
            },
        })
    }).map(|(conn, meta)| (client, conn, meta));
    Box::new(try_connect_all)
}

fn get_original_dest(fd: RawFd) -> io::Result<SocketAddr> {
    let addr = socket::getsockopt(fd, socket::sockopt::OriginalDst)
        .map_err(|e| match e {
            nix::Error::Sys(err) => io::Error::from(err),
            _ => io::Error::new(ErrorKind::Other, e),
        })?;
    let addr = SocketAddrV4::new(addr.sin_addr.s_addr.to_be().into(),
                                 addr.sin_port.to_be());
    // TODO: support IPv6
    Ok(SocketAddr::V4(addr))
}

