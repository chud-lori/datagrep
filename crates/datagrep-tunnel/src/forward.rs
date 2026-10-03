use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::TunnelError;
use crate::host_key::HostKeyPolicy;
use crate::tunnel::SshTunnel;

pub type Connector<P> = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<SshTunnel<P>, TunnelError>> + Send>>
        + Send
        + Sync,
>;

/// A 127.0.0.1 listener whose every accepted socket is carried over one SSH session to a fixed target.
pub struct LocalForward {
    addr: SocketAddr,
    accept: JoinHandle<()>,
}

impl std::fmt::Debug for LocalForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalForward")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl LocalForward {
    pub async fn start<P: HostKeyPolicy + 'static>(
        connect: Connector<P>,
        target_host: String,
        target_port: u16,
    ) -> Result<Self, TunnelError> {
        let session = Arc::new(Mutex::new(connect().await?));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let accept = tokio::spawn(async move {
            loop {
                let socket = match listener.accept().await {
                    Ok((socket, _)) => socket,
                    Err(error) => {
                        tracing::warn!(%error, "tunnel listener accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                tokio::spawn(carry(
                    socket,
                    session.clone(),
                    connect.clone(),
                    target_host.clone(),
                    target_port,
                ));
            }
        });
        Ok(Self { addr, accept })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for LocalForward {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn carry<P: HostKeyPolicy + 'static>(
    mut socket: TcpStream,
    session: Arc<Mutex<SshTunnel<P>>>,
    connect: Connector<P>,
    target_host: String,
    target_port: u16,
) {
    let tunnel = {
        let mut current = session.lock().await;
        if current.is_closed() {
            match connect().await {
                Ok(fresh) => *current = fresh,
                Err(error) => {
                    tracing::warn!(%error, "could not re-open the SSH session");
                    return;
                }
            }
        }
        current.clone()
    };
    match tunnel.open_stream(target_host, target_port).await {
        Ok(mut channel) => {
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut channel).await;
        }
        Err(error) => tracing::warn!(%error, "could not open the forwarded channel"),
    }
}
