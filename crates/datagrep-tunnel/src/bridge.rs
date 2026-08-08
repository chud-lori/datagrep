//! Bridges an SSH `direct-tcpip` channel to an **in-process**
//! `tokio::io::DuplexStream`. The tunnel endpoint is that duplex stream, not
//! a real listening TCP port, so nothing else on the machine can connect to
//! it and ride the tunnel into the remote database.
//!
//! [`crate::SshTunnel::open_channel`] is the only real caller; this module
//! exists standalone so the copy loop can be tested against a mock
//! AsyncRead/AsyncWrite pair instead of a live SSH server.

use tokio::io::{AsyncRead, AsyncWrite};

/// Half of the pair handed to the driver: the local end of the duplex. The
/// other half is spliced to the SSH channel by [`spawn_bridge`].
pub type LocalEnd = tokio::io::DuplexStream;

/// Buffer size for the in-process duplex. Small enough to give real
/// backpressure (the driver can't get more than this many bytes ahead of
/// what's been read off the wire), large enough that typical query-result
/// batches don't thrash it — matches the ballpark of a single TCP socket
/// buffer.
pub const DUPLEX_BUFFER: usize = 64 * 1024;

/// Create a fresh local/remote duplex pair and spawn a task that pumps bytes
/// between `remote` and `channel` in both directions until either side
/// closes. Returns the `LocalEnd` for the caller to hand to the driver as
/// `ConnectCtx::transport`.
///
/// `channel` is generic (not literally `russh::Channel::into_stream()`) so
/// this can be exercised in tests with a mock stream — see the `tests`
/// module.
pub fn spawn_bridge<C>(channel: C) -> LocalEnd
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (local, mut remote) = tokio::io::duplex(DUPLEX_BUFFER);
    tokio::spawn(async move {
        let mut channel = channel;
        // Either direction closing (EOF, error, or the driver dropping its
        // `LocalEnd`) ends the bridge; there is nothing left to do with a
        // half-open direct-tcpip channel here, so errors are swallowed —
        // the driver observes the closed `LocalEnd` and reports its own
        // I/O error up its own stack.
        let _ = tokio::io::copy_bidirectional(&mut remote, &mut channel).await;
    });
    local
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A "channel" stand-in with a deliberately tiny internal buffer, so
    /// tests actually exercise backpressure rather than just correctness.
    fn mock_channel_pair(buf: usize) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        tokio::io::duplex(buf)
    }

    #[tokio::test]
    async fn bytes_flow_both_directions() {
        let (mock_far_end, mock_channel) = mock_channel_pair(DUPLEX_BUFFER);
        let mut local = spawn_bridge(mock_channel);
        let mut far = mock_far_end;

        local.write_all(b"hello from driver").await.unwrap();
        local.flush().await.unwrap();
        let mut buf = vec![0u8; "hello from driver".len()];
        far.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello from driver");

        far.write_all(b"hello from server").await.unwrap();
        far.flush().await.unwrap();
        let mut buf = vec![0u8; "hello from server".len()];
        local.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello from server");
    }

    #[tokio::test]
    async fn fidelity_across_many_small_writes() {
        let (mut far, mock_channel) = mock_channel_pair(DUPLEX_BUFFER);
        let mut local = spawn_bridge(mock_channel);

        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        let writer = {
            let payload = payload.clone();
            tokio::spawn(async move {
                for chunk in payload.chunks(37) {
                    local.write_all(chunk).await.unwrap();
                }
                local.shutdown().await.unwrap();
                local
            })
        };

        let mut received = Vec::new();
        far.read_to_end(&mut received).await.unwrap();
        writer.await.unwrap();
        assert_eq!(
            received, payload,
            "every byte must survive the bridge, in order"
        );
    }

    #[tokio::test]
    async fn backpressure_with_a_small_buffer_does_not_lose_or_corrupt_data() {
        // A buffer far smaller than the payload forces the bridge to
        // actually block/resume repeatedly instead of copying in one shot.
        const TINY: usize = 64;
        let (mut far, mock_channel) = mock_channel_pair(TINY);
        let mut local = spawn_bridge(mock_channel);

        let payload = vec![0xABu8; 200_000];
        let writer = {
            let payload = payload.clone();
            tokio::spawn(async move {
                local.write_all(&payload).await.unwrap();
                local.shutdown().await.unwrap();
            })
        };
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            far.read_to_end(&mut received).await.unwrap();
            received
        });

        writer.await.unwrap();
        let received = reader.await.unwrap();
        assert_eq!(received.len(), payload.len());
        assert!(received.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn closing_the_local_end_ends_the_bridge() {
        let (far, mock_channel) = mock_channel_pair(DUPLEX_BUFFER);
        let local = spawn_bridge(mock_channel);
        drop(local);
        // The far end must observe EOF once the bridge task notices the
        // local side is gone and shuts the channel down.
        let mut far = far;
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), far.read(&mut buf))
            .await
            .expect("bridge should close promptly, not hang")
            .unwrap();
        assert_eq!(n, 0, "expected EOF");
    }
}
