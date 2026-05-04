use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// Linux TCP states from include/net/tcp_states.h
const TCP_CLOSE_WAIT: u8 = 8;
const TCP_LAST_ACK: u8 = 9;
const TCP_CLOSE: u8 = 7;
const TCP_CLOSING: u8 = 11;

/// Per-connection handle that can detect client disconnect by inspecting the
/// TCP socket state via `getsockopt(TCP_INFO)`.
///
/// The raw fd is valid for the lifetime of the hyper connection task that owns
/// the underlying `TcpStream`. Handlers running inside that task can safely
/// check this handle.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    raw_fd: RawFd,
    disconnected: std::sync::Arc<AtomicBool>,
}

impl ConnectionHandle {
    pub fn new(raw_fd: RawFd) -> Self {
        Self {
            raw_fd,
            disconnected: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns true if the client has disconnected (TCP state is CLOSE-WAIT or
    /// similar terminal state). Uses a cached flag so repeated calls after the
    /// first detection are free.
    pub fn is_client_gone(&self) -> bool {
        if self.disconnected.load(Ordering::Relaxed) {
            return true;
        }
        if tcp_state_is_closed(self.raw_fd) {
            self.disconnected.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Async future that resolves when the client disconnects. Polls the TCP socket
/// state every `interval`. Intended for use in `tokio::select!` alongside
/// blocking waits like `capacity_notify.notified()`.
pub async fn poll_client_disconnect(handle: &ConnectionHandle, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        if handle.is_client_gone() {
            return;
        }
    }
}

fn tcp_state_is_closed(fd: RawFd) -> bool {
    unsafe {
        let mut info: libc::tcp_info = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
        let ret = libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            &mut info as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if ret != 0 {
            return true;
        }
        matches!(
            info.tcpi_state,
            TCP_CLOSE_WAIT | TCP_LAST_ACK | TCP_CLOSE | TCP_CLOSING
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[tokio::test]
    async fn test_connected_socket_not_reported_as_gone() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let handle = ConnectionHandle::new(server.as_raw_fd());
        assert!(!handle.is_client_gone());

        drop(client);
        drop(server);
    }

    #[tokio::test]
    async fn test_disconnected_socket_detected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let handle = ConnectionHandle::new(server.as_raw_fd());

        // Client disconnects
        drop(client);

        // Give the kernel a moment to process the FIN
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(handle.is_client_gone());
    }

    #[tokio::test]
    async fn test_poll_client_disconnect_resolves_on_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let handle = ConnectionHandle::new(server.as_raw_fd());

        // Drop client after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(client);
        });

        // poll_client_disconnect should resolve within interval + detection time
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            poll_client_disconnect(&handle, Duration::from_millis(100)),
        )
        .await;

        assert!(result.is_ok(), "poll_client_disconnect should have resolved");
    }

    #[test]
    fn test_invalid_fd_reports_gone() {
        let handle = ConnectionHandle::new(-1);
        assert!(handle.is_client_gone());
    }

    #[tokio::test]
    async fn test_cached_flag_avoids_repeated_syscalls() {
        let handle = ConnectionHandle::new(-1);
        assert!(handle.is_client_gone());
        // Second call uses cached flag
        assert!(handle.is_client_gone());
        assert!(handle.disconnected.load(Ordering::Relaxed));
    }
}
