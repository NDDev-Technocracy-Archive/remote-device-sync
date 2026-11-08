//! One bounded body with an integrity trailer. Callers own authorization,
//! native source/destination handles, stream priority and publication. This
//! core never opens a path, retains a file cache or allocates the whole body.
use rds_core::clipboard::{Limits, PayloadKind, Receipt, Request, TransferHeader};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
mod reader;
pub use reader::receive_reader;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error(transparent)]
    Source(#[from] super::native::Error),
    #[error("clipboard body I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("clipboard body length differs from its selection")]
    Length,
    #[error("clipboard body integrity mismatch")]
    Integrity,
}

/// Copy from an already selected, bounded native reader. The caller's single
/// owner keeps that reader and the authenticated session alive until completion
/// or cancellation drain. Nothing reads beyond the admitted selected length.
pub async fn send<R, W>(
    source: &mut R,
    wire: &mut W,
    header: &TransferHeader,
    limits: &Limits,
) -> Result<Receipt, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    send_checked(source, wire, header, limits, async { Ok(()) }).await
}

/// Receive a selected file directly into the receiving application's newly
/// opened native destination. The destination retains the original paste
/// operation through actual native I/O, including a canceled waiting future.
/// Its owner still controls publication and failure cleanup.
pub async fn receive_file<R: AsyncRead + Unpin + Send>(
    wire: &mut R,
    destination: super::native::Destination,
    expected: &Request,
    limits: &Limits,
) -> Result<Receipt, Error> {
    expected.validate(limits)?;
    if !matches!(expected.kind, PayloadKind::File(_)) {
        return Err(rds_core::clipboard::Error::Identity.into());
    }
    receive_body(wire, &mut NativeFile(destination), expected, limits).await
}

trait Sink: Send {
    fn begin(&mut self) -> impl Future<Output = Result<(), Error>> + Send;
    fn write(
        &mut self,
        chunk: Vec<u8>,
        length: usize,
        offset: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send;
    fn finish(&mut self) -> impl Future<Output = Result<(), Error>> + Send;
}

struct Writer<'a, W>(&'a mut W);
impl<W: AsyncWrite + Unpin + Send> Sink for Writer<'_, W> {
    async fn begin(&mut self) -> Result<(), Error> {
        Ok(())
    }
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        length: usize,
        _offset: u64,
    ) -> Result<Vec<u8>, Error> {
        self.0.write_all(&chunk[..length]).await?;
        Ok(chunk)
    }
    async fn finish(&mut self) -> Result<(), Error> {
        self.0.flush().await?;
        Ok(())
    }
}

struct NativeFile(super::native::Destination);
impl Sink for NativeFile {
    async fn begin(&mut self) -> Result<(), Error> {
        Ok(self.0.validate_empty().await?)
    }
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        length: usize,
        offset: u64,
    ) -> Result<Vec<u8>, Error> {
        Ok(self.0.write_chunk(chunk, length, offset).await?)
    }
    async fn finish(&mut self) -> Result<(), Error> {
        // Each native syscall has returned before its write future completes.
        Ok(())
    }
}

/// Forward a selected or remote-origin file without staging it. The source's
/// own version/integrity proof must finish before this stream gets a trailer.
pub async fn send_file<W: AsyncWrite + Unpin>(
    source: super::native::FileRead,
    wire: &mut W,
    header: &TransferHeader,
    limits: &Limits,
) -> Result<Receipt, Error> {
    let super::native::FileRead {
        bytes,
        mut reader,
        complete,
    } = source;
    if bytes != header.bytes || !matches!(header.kind, PayloadKind::File(_)) {
        return Err(rds_core::clipboard::Error::Identity.into());
    }
    send_checked(&mut reader, wire, header, limits, complete).await
}

async fn send_checked<R, W>(
    source: &mut R,
    wire: &mut W,
    header: &TransferHeader,
    limits: &Limits,
    complete: impl Future<Output = Result<(), super::native::Error>>,
) -> Result<Receipt, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    header.validate(&header.request(), limits)?;
    rds_net::write_frame(wire, header).await?;
    let mut remaining = header.bytes;
    let mut chunk = vec![0; limits.chunk_bytes as usize];
    let mut hash = blake3::Hasher::new();
    while remaining > 0 {
        let maximum = remaining.min(chunk.len() as u64) as usize;
        let length = source.read(&mut chunk[..maximum]).await?;
        if length == 0 {
            return Err(Error::Length);
        }
        hash.update(&chunk[..length]);
        wire.write_all(&chunk[..length]).await?;
        remaining -= length as u64;
    }
    complete.await?;
    let digest = *hash.finalize().as_bytes();
    wire.write_all(&digest).await?;
    wire.flush().await?;
    Ok(Receipt {
        id: header.id,
        bytes: header.bytes,
        digest,
        published: false,
    })
}

/// A destination stays unpublished until the complete exact body, trailer and
/// EOF validate. On any failure its native owner discards the unpublished
/// object. There is no chunk-cache write or second full receiver-side copy.
pub async fn receive<R, W>(
    wire: &mut R,
    destination: &mut W,
    expected: &Request,
    limits: &Limits,
) -> Result<Receipt, Error>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    receive_body(wire, &mut Writer(destination), expected, limits).await
}

async fn receive_body<R: AsyncRead + Unpin + Send>(
    wire: &mut R,
    destination: &mut impl Sink,
    expected: &Request,
    limits: &Limits,
) -> Result<Receipt, Error> {
    // Validate the locally admitted request before reading any peer payload.
    expected.validate(limits)?;
    let header: TransferHeader = rds_net::read_frame(wire).await?;
    header.validate(expected, limits)?;
    destination.begin().await?;
    let (mut body, proof) = reader::body(wire, header.bytes, None);
    let mut remaining = header.bytes;
    let mut chunk = vec![0; limits.chunk_bytes as usize];
    while remaining > 0 {
        let maximum = remaining.min(chunk.len() as u64) as usize;
        let length = body.read(&mut chunk[..maximum]).await?;
        if length == 0 {
            return Err(Error::Length);
        }
        chunk = destination
            .write(chunk, length, header.bytes - remaining)
            .await?;
        remaining -= length as u64;
    }
    let digest = proof.await?;
    destination.finish().await?;
    Ok(Receipt {
        id: header.id,
        bytes: header.bytes,
        digest,
        published: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rds_core::clipboard::{Format, PayloadKind, TransferId};

    fn header(bytes: u64) -> TransferHeader {
        TransferHeader {
            id: TransferId {
                desktop: [1; 16],
                offer: 2,
                request: 3,
            },
            kind: PayloadKind::Representation(Format::TextUtf8),
            bytes,
        }
    }

    #[tokio::test]
    async fn bounded_duplex_transfers_empty_and_multichunk_bodies_without_consuming_the_source_tail()
     {
        let limits = Limits {
            chunk_bytes: 7,
            ..Limits::default()
        };
        for value in [b"".as_slice(), "Привет 🖥️\n".repeat(100).as_bytes()] {
            let expected = header(value.len() as u64);
            let source = [value, b"unselected tail"].concat();
            let mut source = source.as_slice();
            let (mut outgoing, mut incoming) = tokio::io::duplex(11);
            let mut destination = Vec::new();
            let sending = async {
                let receipt = send(&mut source, &mut outgoing, &expected, &limits).await?;
                outgoing.shutdown().await?;
                Ok::<_, Error>(receipt)
            };
            let request = expected.request();
            let receiving = receive(&mut incoming, &mut destination, &request, &limits);
            let (sent, received) = tokio::join!(sending, receiving);
            assert_eq!(sent.unwrap(), received.unwrap());
            assert_eq!(destination, value);
            assert_eq!(source, b"unselected tail");
        }
    }

    #[tokio::test]
    async fn wrong_request_never_writes_a_native_destination() {
        let limits = Limits::default();
        let expected = header(3);
        let mut wrong = expected.clone();
        wrong.id.request += 1;
        let mut wire = Vec::new();
        send(&mut b"abc".as_slice(), &mut wire, &wrong, &limits)
            .await
            .unwrap();
        let mut destination = Vec::new();
        assert!(matches!(
            receive(
                &mut wire.as_slice(),
                &mut destination,
                &expected.request(),
                &limits
            )
            .await,
            Err(Error::Contract(_))
        ));
        assert!(destination.is_empty());
    }

    #[tokio::test]
    async fn a_selected_file_stream_writes_one_new_native_destination_and_refuses_overwrite() {
        let root =
            std::env::temp_dir().join(format!("rds-file-stream-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("pasted");
        let limits = Limits {
            chunk_bytes: 4096,
            ..Limits::default()
        };
        let value = "Привет 🖥️\n".repeat(10000).into_bytes();
        let mut expected = header(value.len() as u64);
        expected.kind = PayloadKind::File([4; 16]);
        let request = expected.request();
        let output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        let destination = super::super::native::Destination::new(output, std::sync::Arc::new(()));
        let (mut outgoing, mut incoming) = tokio::io::duplex(8192);
        let sending = async {
            let receipt = send(&mut value.as_slice(), &mut outgoing, &expected, &limits).await?;
            outgoing.shutdown().await?;
            Ok::<_, Error>(receipt)
        };
        let receiving = receive_file(&mut incoming, destination, &request, &limits);
        let (sent, received) = tokio::join!(sending, receiving);
        let receipt = received.unwrap();
        assert_eq!(sent.unwrap(), receipt);
        assert!(!receipt.published);
        assert_eq!(std::fs::read(&path).unwrap(), value);

        let mut wire = Vec::new();
        send(&mut value.as_slice(), &mut wire, &expected, &limits)
            .await
            .unwrap();
        let output = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(matches!(
            receive_file(
                &mut wire.as_slice(),
                super::super::native::Destination::new(output, std::sync::Arc::new(())),
                &request,
                &limits,
            )
            .await,
            Err(Error::Source(super::super::native::Error::Io(_)))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), value);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_complete_body_is_unpublished_when_corrupted_truncated_or_extended() {
        let limits = Limits::default();
        let expected = header(3);
        let mut complete = Vec::new();
        send(&mut b"abc".as_slice(), &mut complete, &expected, &limits)
            .await
            .unwrap();
        let mut corrupt = complete.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let mut extended = complete.clone();
        extended.push(0);
        let truncated = complete[..complete.len() - 1].to_vec();
        for bytes in [corrupt, extended, truncated] {
            let mut destination = Vec::new();
            assert!(
                receive(
                    &mut bytes.as_slice(),
                    &mut destination,
                    &expected.request(),
                    &limits
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn an_upstream_failure_cannot_be_forwarded_as_a_successful_file() {
        let mut header = header(3);
        header.kind = PayloadKind::File([2; 16]);
        let source = super::super::native::FileRead {
            bytes: 3,
            reader: Box::pin(b"abc".as_slice()),
            complete: Box::pin(async { Err(super::super::native::Error::Changed) }),
        };
        let mut wire = Vec::new();
        let limits = Limits::default();
        assert!(matches!(
            send_file(source, &mut wire, &header, &limits).await,
            Err(Error::Source(_))
        ));
        let mut destination = Vec::new();
        assert!(
            receive(
                &mut wire.as_slice(),
                &mut destination,
                &header.request(),
                &limits
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn remote_files_forward_without_staging_and_cannot_hide_a_bad_upstream_proof() {
        let root = std::env::temp_dir().join(format!(
            "rds-forwarded-file-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let limits = Limits {
            chunk_bytes: 4096,
            ..Limits::default()
        };
        let value = "Привет 🖥️\n".repeat(10000).into_bytes();
        let mut upstream = header(value.len() as u64);
        upstream.kind = PayloadKind::File([4; 16]);
        let mut downstream = upstream.clone();
        downstream.id.desktop = [5; 16];
        downstream.id.request += 1;
        for corrupt in [false, true] {
            let mut wire = Vec::new();
            send(&mut value.as_slice(), &mut wire, &upstream, &limits)
                .await
                .unwrap();
            if corrupt {
                *wire.last_mut().unwrap() ^= 1;
            }
            let context = std::sync::Arc::new(());
            let original = std::sync::Arc::downgrade(&context);
            let source = receive_reader(
                std::io::Cursor::new(wire),
                &upstream.request(),
                &limits,
                context,
            )
            .await
            .unwrap();
            assert!(original.upgrade().is_some());
            let path = root.join(if corrupt { "unpublished" } else { "pasted" });
            let output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .unwrap();
            let destination =
                super::super::native::Destination::new(output, std::sync::Arc::new(()));
            let (mut outgoing, mut incoming) = tokio::io::duplex(8192);
            let sending = async {
                let result = send_file(source, &mut outgoing, &downstream, &limits).await;
                outgoing.shutdown().await.unwrap();
                result
            };
            let request = downstream.request();
            let receiving = receive_file(&mut incoming, destination, &request, &limits);
            let (sent, received) = tokio::join!(sending, receiving);
            if corrupt {
                assert!(matches!(
                    sent,
                    Err(Error::Source(super::super::native::Error::Integrity))
                ));
                assert!(received.is_err());
            } else {
                assert_eq!(sent.unwrap(), received.unwrap());
            }
            assert!(original.upgrade().is_none());
            assert_eq!(std::fs::read(&path).unwrap(), value);
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
