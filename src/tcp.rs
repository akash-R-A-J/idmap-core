use anyhow::Result;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};

pub async fn accept(
    listener: &TcpListener,
    secs: u64,
) -> Result<(TcpStream, SocketAddr)> {
    let (s, a) = timeout(Duration::from_secs(secs), listener.accept()).await??;
    Ok((s, a))
}

pub async fn connect(addr: &str) -> Result<TcpStream> {
    Ok(TcpStream::connect(addr).await?)
}
