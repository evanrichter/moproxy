mod connect;
mod read;
mod tls;
use futures::{future, Future};
use futures03::future::{FutureExt, TryFutureExt};
use log::{debug, info, warn};
use std::cmp;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;

use crate::{
    client::connect::try_connect_all,
    client::read::read_with_timeout,
    client::tls::parse_client_hello,
    monitor::ServerList,
    proxy::copy::{pipe, SharedBuf},
    proxy::{Destination, ProxyServer},
    tcp::{get_original_dest, get_original_dest6},
    RcBox,
};

#[derive(Debug)]
pub struct NewClient {
    left: TcpStream,
    src: SocketAddr,
    pub dest: Destination,
    list: ServerList,
    handle: Handle,
}

#[derive(Debug)]
pub struct NewClientWithData {
    client: NewClient,
    pending_data: Option<Box<[u8]>>,
    allow_parallel: bool,
}

#[derive(Debug)]
pub struct ConnectedClient {
    left: TcpStream,
    right: TcpStream,
    dest: Destination,
    server: Arc<ProxyServer>,
}

type ConnectServer = Box<dyn Future<Item = ConnectedClient, Error = ()>>;

pub trait Connectable {
    fn connect_server(self, n_parallel: usize) -> ConnectServer;
}

impl NewClient {
    pub fn from_socket(
        left: TcpStream,
        list: ServerList,
        handle: Handle,
    ) -> impl Future<Item = Self, Error = ()> {
        let dest4 = future::result(get_original_dest(&left)).map(SocketAddr::V4);
        let dest6 = future::result(get_original_dest6(&left)).map(SocketAddr::V6);
        // TODO: call either v6 or v4 according to our socket
        let src_dest = future::result(left.peer_addr())
            .join(dest4.or_else(|_| dest6))
            .map_err(|err| warn!("fail to get original dest: {}", err));
        src_dest.map(move |(src, dest)| {
            debug!("dest {:?}", dest);
            NewClient {
                left,
                src,
                dest: dest.into(),
                list,
                handle,
            }
        })
    }
}

impl NewClient {
    pub fn retrive_dest(self) -> impl Future<Item = NewClientWithData, Error = ()> {
        let NewClient {
            left,
            src,
            mut dest,
            list,
            handle,
        } = self;
        let wait = Duration::from_millis(500);
        // try to read TLS ClientHello for
        //   1. --remote-dns: parse host name from SNI
        //   2. --n-parallel: need the whole request to be forwarded
        let read = read_with_timeout(left, vec![0u8; 2048], wait, &handle).compat();
        read.map(move |(left, mut data, len)| {
            let (allow_parallel, pending_data) = if len == 0 {
                info!("no tls request received before timeout");
                (false, None)
            } else {
                data.truncate(len);
                // only TLS is safe to duplicate requests.
                let allow_parallel = match parse_client_hello(&data) {
                    Err(err) => {
                        info!("fail to parse hello: {}", err);
                        false
                    }
                    Ok(hello) => {
                        if let Some(name) = hello.server_name {
                            dest = (name, dest.port).into();
                            debug!("SNI found: {}", name);
                        }
                        if hello.early_data {
                            debug!("TLS with early data");
                        }
                        true
                    }
                };
                (allow_parallel, Some(data.into_boxed_slice()))
            };
            NewClientWithData {
                client: NewClient {
                    left,
                    src,
                    dest,
                    list,
                    handle,
                },
                allow_parallel,
                pending_data,
            }
        })
        .map_err(|err| warn!("fail to read hello from client: {}", err))
    }

    fn connect_server(
        self,
        n_parallel: usize,
        wait_response: bool,
        pending_data: Option<Box<[u8]>>,
    ) -> ConnectServer {
        let NewClient {
            left,
            src,
            dest,
            list,
            handle,
        } = self;
        let pending_data = pending_data.map(RcBox::new);
        let conn = try_connect_all(
            dest.clone(),
            list,
            n_parallel,
            wait_response,
            pending_data,
            handle,
        );
        let client = conn
            .map(move |(server, right)| {
                info!("{} => {} via {}", src, dest, server.tag);
                ConnectedClient {
                    left,
                    right,
                    dest,
                    server,
                }
            })
            .map_err(|_| warn!("all proxy server down"));
        Box::new(client)
    }
}

impl Connectable for NewClient {
    fn connect_server(self, _n_parallel: usize) -> ConnectServer {
        self.connect_server(1, false, None)
    }
}

impl Connectable for NewClientWithData {
    fn connect_server(self, n_parallel: usize) -> ConnectServer {
        let NewClientWithData {
            client,
            pending_data,
            allow_parallel,
        } = self;
        let n_parallel = if allow_parallel {
            cmp::min(client.list.len(), n_parallel)
        } else {
            1
        };
        client.connect_server(n_parallel, true, pending_data)
    }
}

impl ConnectedClient {
    pub fn serve(self, shared_buf: SharedBuf) -> impl Future<Item = (), Error = ()> {
        let ConnectedClient {
            left,
            right,
            dest,
            server,
        } = self;
        // TODO: make keepalive configurable
        let timeout = Some(Duration::from_secs(300));
        if let Err(e) = left
            .set_keepalive(timeout)
            .and(right.set_keepalive(timeout))
        {
            warn!("fail to set keepalive: {}", e);
        }

        server.update_stats_conn_open();
        pipe(left, right, server.clone(), shared_buf).then(move |result| match result {
            Ok(amt) => {
                server.update_stats_conn_close(false);
                debug!(
                    "tx {}, rx {} bytes ({} => {})",
                    amt.tx_bytes, amt.rx_bytes, server.tag, dest
                );
                Ok(())
            }
            Err(_) => {
                server.update_stats_conn_close(true);
                warn!("{} (=> {}) close with error", server.tag, dest);
                Err(())
            }
        })
    }
}
