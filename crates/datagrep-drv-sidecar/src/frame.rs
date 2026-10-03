use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Four times the default FetchHint.max_bytes.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Oversized(usize),
    Truncated,
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Oversized(len) => {
                write!(f, "frame of {len} bytes exceeds the {MAX_FRAME}-byte cap")
            }
            FrameError::Truncated => f.write_str("stream ended inside a frame"),
            FrameError::Io(e) => write!(f, "pipe error: {e}"),
        }
    }
}

// Ok(None) is a clean EOF on a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    cap: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]).await.map_err(FrameError::Io)? {
            0 if got == 0 => return Ok(None),
            0 => return Err(FrameError::Truncated),
            n => got += n,
        }
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > cap {
        return Err(FrameError::Oversized(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(e),
    })?;
    Ok(Some(body))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| std::io::Error::other("outgoing frame exceeds the cap"))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_reports_clean_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"id":1}"#).await.unwrap();
        write_frame(&mut buf, b"").await.unwrap();
        let mut r = buf.as_slice();
        assert_eq!(
            read_frame(&mut r, MAX_FRAME).await.unwrap().unwrap(),
            br#"{"id":1}"#
        );
        assert_eq!(read_frame(&mut r, MAX_FRAME).await.unwrap().unwrap(), b"");
        assert!(read_frame(&mut r, MAX_FRAME).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_an_oversized_header_before_reading_the_body() {
        // Only the header exists: a reader that allocated first would block or abort here.
        let header = (u32::MAX).to_be_bytes();
        let mut r = &header[..];
        match read_frame(&mut r, MAX_FRAME).await {
            Err(FrameError::Oversized(n)) => assert_eq!(n, u32::MAX as usize),
            other => panic!("expected Oversized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_short_body_or_header_is_truncation_not_eof() {
        let mut r: &[u8] = &[0, 0];
        assert!(matches!(
            read_frame(&mut r, MAX_FRAME).await,
            Err(FrameError::Truncated)
        ));
        let mut r: &[u8] = &[0, 0, 0, 5, b'{'];
        assert!(matches!(
            read_frame(&mut r, MAX_FRAME).await,
            Err(FrameError::Truncated)
        ));
    }
}
