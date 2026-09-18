// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::rpc::error::RpcError;
use socket2::{SockRef, TcpKeepalive};
use std::ops::DerefMut;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

#[derive(Debug)]
pub enum Transport {
    Plain { inner: TcpStream },
}

impl AsyncRead for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.deref_mut() {
            Self::Plain { inner } => Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.deref_mut() {
            Self::Plain { inner } => Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.deref_mut() {
            Self::Plain { inner } => Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.deref_mut() {
            Self::Plain { inner } => Pin::new(inner).poll_shutdown(cx),
        }
    }
}

impl Transport {
    pub async fn connect(server: &str, timeout: Option<Duration>) -> Result<Self, RpcError> {
        let tcp_stream = Self::connect_timeout(server, timeout).await?;
        Self::configure_socket(&tcp_stream);
        Ok(Transport::Plain { inner: tcp_stream })
    }

    /// Best-effort socket tuning applied right after connect. TCP keepalive lets
    /// the OS detect a dead peer on an otherwise idle connection, complementing
    /// the application-level idle timeout. Failures are logged, not fatal, so a
    /// platform that rejects an option never blocks connecting. Mirrors Java's
    /// `SO_KEEPALIVE` / `TCP_NODELAY` client socket options.
    fn configure_socket(stream: &TcpStream) {
        let sock_ref = SockRef::from(stream);
        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10));
        if let Err(e) = sock_ref.set_tcp_keepalive(&keepalive) {
            log::warn!("Failed to enable TCP keepalive: {e}");
        }
        if let Err(e) = sock_ref.set_tcp_nodelay(true) {
            log::warn!("Failed to enable TCP_NODELAY: {e}");
        }
    }

    async fn connect_timeout(host: &str, timeout: Option<Duration>) -> Result<TcpStream, RpcError> {
        match timeout {
            Some(timeout) => Ok(tokio::time::timeout(timeout, TcpStream::connect(host))
                .await
                .map_err(|_| {
                    RpcError::ConnectionError(format!("Timeout connecting to host {host}"))
                })??),
            None => Ok(TcpStream::connect(host).await?),
        }
    }
}
