use std::{
    io::{self, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc,
    task,
};

use crate::{domain::Sha256Digest, infrastructure::cancellation::CancellationToken};

pub const DEFAULT_ZSTD_LEVEL: i32 = 1;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const BUFFERED_CHUNKS: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionMetrics {
    pub(crate) input_bytes: u64,
    pub(crate) compressed_bytes: u64,
    pub(crate) input_sha256: Sha256Digest,
    pub(crate) compressed_sha256: Sha256Digest,
    pub(crate) elapsed: Duration,
}

impl CompressionMetrics {
    pub const fn input_bytes(&self) -> u64 {
        self.input_bytes
    }

    pub const fn compressed_bytes(&self) -> u64 {
        self.compressed_bytes
    }

    pub const fn input_sha256(&self) -> Sha256Digest {
        self.input_sha256
    }

    pub const fn compressed_sha256(&self) -> Sha256Digest {
        self.compressed_sha256
    }

    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub fn average_input_bytes_per_second(&self) -> f64 {
        rate(self.input_bytes, self.elapsed)
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.input_bytes == 0 {
            return 0.0;
        }
        self.compressed_bytes as f64 / self.input_bytes as f64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionProgress {
    pub input_bytes: u64,
    pub elapsed: Duration,
}

impl CompressionProgress {
    pub fn input_bytes_per_second(self) -> f64 {
        rate(self.input_bytes, self.elapsed)
    }
}

pub trait CompressionProgressObserver: Send + Sync {
    fn set_estimated_input_bytes(&self, _estimated_input_bytes: u64) {}

    fn update(&self, progress: CompressionProgress);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoCompressionProgress;

impl CompressionProgressObserver for NoCompressionProgress {
    fn update(&self, _progress: CompressionProgress) {}
}

#[derive(Clone, Copy, Debug)]
pub struct ZstdCompressor {
    level: i32,
}

impl Default for ZstdCompressor {
    fn default() -> Self {
        Self::new(DEFAULT_ZSTD_LEVEL)
    }
}

impl ZstdCompressor {
    pub const fn new(level: i32) -> Self {
        Self { level }
    }

    pub async fn compress<R, W>(
        &self,
        input: R,
        output: W,
        observer: Arc<dyn CompressionProgressObserver>,
    ) -> Result<CompressionMetrics, CompressionError>
    where
        R: AsyncRead + Unpin,
        W: Write + Send + 'static,
    {
        self.compress_with_cancellation(input, output, observer, CancellationToken::default())
            .await
    }

    pub async fn compress_with_cancellation<R, W>(
        &self,
        mut input: R,
        output: W,
        observer: Arc<dyn CompressionProgressObserver>,
        cancellation: CancellationToken,
    ) -> Result<CompressionMetrics, CompressionError>
    where
        R: AsyncRead + Unpin,
        W: Write + Send + 'static,
    {
        let started_at = Instant::now();
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(BUFFERED_CHUNKS);
        let level = self.level;
        let worker = task::spawn_blocking(move || encode_chunks(receiver, output, level));
        let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
        let mut input_bytes = 0_u64;

        let read_result = loop {
            let bytes_read = match tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(CompressionError::Interrupted),
                result = input.read(&mut buffer) => result.map_err(CompressionError::ReadInput),
            } {
                Ok(0) => break Ok(()),
                Ok(bytes_read) => bytes_read,
                Err(error) => break Err(error),
            };
            input_bytes = input_bytes
                .checked_add(bytes_read as u64)
                .ok_or(CompressionError::InputTooLarge)?;
            if sender.send(buffer[..bytes_read].to_vec()).await.is_err() {
                break Err(CompressionError::EncoderStopped);
            }
            observer.update(CompressionProgress {
                input_bytes,
                elapsed: started_at.elapsed(),
            });
        };
        drop(sender);

        let encoded = worker
            .await
            .map_err(|_| CompressionError::EncoderTaskFailed)?;
        let encoded = match (read_result, encoded) {
            (_, Err(error)) => return Err(error),
            (Err(error), Ok(_)) => return Err(error),
            (Ok(()), Ok(encoded)) => encoded,
        };

        Ok(CompressionMetrics {
            input_bytes,
            compressed_bytes: encoded.compressed_bytes,
            input_sha256: encoded.input_sha256,
            compressed_sha256: encoded.compressed_sha256,
            elapsed: started_at.elapsed(),
        })
    }
}

#[derive(Debug, Error)]
pub enum CompressionError {
    #[error("could not read the dump stream")]
    ReadInput(#[source] io::Error),

    #[error("the dump stream is too large to count safely")]
    InputTooLarge,

    #[error("the Zstd encoder stopped before consuming the complete dump stream")]
    EncoderStopped,

    #[error("the Zstd encoder task terminated unexpectedly")]
    EncoderTaskFailed,

    #[error("dump compression interrupted")]
    Interrupted,

    #[error(
        "could not create the compressed dump; verify free space and permissions in the reprodb home"
    )]
    Encode(#[source] io::Error),
}

struct EncodedOutput {
    compressed_bytes: u64,
    input_sha256: Sha256Digest,
    compressed_sha256: Sha256Digest,
}

fn encode_chunks<W>(
    mut receiver: mpsc::Receiver<Vec<u8>>,
    output: W,
    level: i32,
) -> Result<EncodedOutput, CompressionError>
where
    W: Write,
{
    let output = CountingHashWriter::new(output);
    let mut encoder =
        zstd::stream::write::Encoder::new(output, level).map_err(CompressionError::Encode)?;
    let mut input_hasher = Sha256::new();

    while let Some(chunk) = receiver.blocking_recv() {
        input_hasher.update(&chunk);
        encoder
            .write_all(&chunk)
            .map_err(CompressionError::Encode)?;
    }

    let output = encoder.finish().map_err(CompressionError::Encode)?;
    let (compressed_bytes, compressed_sha256) = output.finish();
    Ok(EncodedOutput {
        compressed_bytes,
        input_sha256: digest(input_hasher.finalize()),
        compressed_sha256,
    })
}

struct CountingHashWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W> CountingHashWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (u64, Sha256Digest) {
        (self.bytes, digest(self.hasher.finalize()))
    }
}

impl<W> Write for CountingHashWriter<W>
where
    W: Write,
{
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.bytes = self
            .bytes
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("compressed byte counter overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn digest(digest: impl Into<[u8; 32]>) -> Sha256Digest {
    Sha256Digest::from_bytes(digest.into())
}

fn rate(bytes: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        return 0.0;
    }
    bytes as f64 / seconds
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::Cursor,
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };

    use tempfile::tempdir;
    use tokio::io::ReadBuf;

    use super::*;

    #[derive(Default)]
    struct RecordingProgress {
        updates: Mutex<Vec<CompressionProgress>>,
    }

    impl CompressionProgressObserver for RecordingProgress {
        fn update(&self, progress: CompressionProgress) {
            self.updates.lock().unwrap().push(progress);
        }
    }

    #[tokio::test]
    async fn compresses_and_decompresses_a_stream_with_exact_metrics() {
        let mut pattern = "acme_production\nUTF-8: olá 🧂\n\0binary"
            .as_bytes()
            .to_vec();
        pattern.push(0xff);
        let input = pattern.repeat(1024);
        let directory = tempdir().unwrap();
        let output = directory.path().join("dump.sql.zst");
        let progress = Arc::new(RecordingProgress::default());
        let file = File::create(&output).unwrap();

        let metrics = ZstdCompressor::default()
            .compress(Cursor::new(input.clone()), file, progress.clone())
            .await
            .unwrap();

        let compressed = std::fs::read(&output).unwrap();
        let decoded = zstd::stream::decode_all(Cursor::new(&compressed)).unwrap();
        assert_eq!(decoded, input);
        assert_eq!(metrics.input_bytes, input.len() as u64);
        assert_eq!(metrics.compressed_bytes, compressed.len() as u64);
        assert_eq!(metrics.input_sha256, digest(Sha256::digest(&input)));
        assert_eq!(
            metrics.compressed_sha256,
            digest(Sha256::digest(&compressed))
        );
        assert!(metrics.average_input_bytes_per_second().is_finite());
        assert!(metrics.compression_ratio() > 0.0);
        let updates = progress.updates.lock().unwrap();
        assert_eq!(updates.last().unwrap().input_bytes, input.len() as u64);
        assert!(updates.last().unwrap().input_bytes_per_second().is_finite());
    }

    #[tokio::test]
    async fn consumes_a_large_generated_input_using_only_fixed_size_reads() {
        let total_bytes = 32 * 1024 * 1024;
        let reader = GeneratedReader::new(total_bytes);
        let max_requested = Arc::clone(&reader.max_requested);
        let output = SharedWriter::default();
        let compressed = Arc::clone(&output.bytes);

        let metrics = ZstdCompressor::default()
            .compress(reader, output, Arc::new(NoCompressionProgress))
            .await
            .unwrap();

        assert_eq!(metrics.input_bytes, total_bytes as u64);
        assert_eq!(*max_requested.lock().unwrap(), STREAM_BUFFER_BYTES);
        let decoded =
            zstd::stream::decode_all(Cursor::new(compressed.lock().unwrap().clone())).unwrap();
        assert_eq!(decoded.len(), total_bytes);
        assert!(decoded.iter().all(|byte| *byte == b'x'));
    }

    #[tokio::test]
    #[ignore = "manual local comparison of candidate Zstd levels"]
    async fn benchmarks_candidate_zstd_levels_on_sql_like_input() {
        let row = b"INSERT INTO `work_orders` VALUES (123,'acme_production','2026-09-05 12:34:56',NULL,1234.56);\n";
        let repetitions = (64 * 1024 * 1024) / row.len();
        let input = row.repeat(repetitions);

        for level in [1, 3] {
            let metrics = ZstdCompressor::new(level)
                .compress(
                    Cursor::new(input.as_slice()),
                    SharedWriter::default(),
                    Arc::new(NoCompressionProgress),
                )
                .await
                .unwrap();
            assert_eq!(metrics.input_bytes, input.len() as u64);
            eprintln!(
                "level={level} input={} compressed={} ratio={:.4} throughput_mib_s={:.1}",
                metrics.input_bytes,
                metrics.compressed_bytes,
                metrics.compression_ratio(),
                metrics.average_input_bytes_per_second() / (1024.0 * 1024.0),
            );
        }
    }

    #[tokio::test]
    async fn reports_a_full_disk_without_claiming_compression_success() {
        let error = ZstdCompressor::default()
            .compress(
                Cursor::new(vec![b'x'; STREAM_BUFFER_BYTES * 4]),
                FailingWriter { remaining: 8 },
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            CompressionError::Encode(ref source)
                if source.kind() == io::ErrorKind::StorageFull
        ));
    }

    #[tokio::test]
    async fn preserves_an_input_read_failure_after_finishing_the_partial_frame() {
        let error = ZstdCompressor::default()
            .compress(
                FailingReader { emitted: false },
                SharedWriter::default(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, CompressionError::ReadInput(_)));
    }

    #[tokio::test]
    async fn cancellation_finishes_the_encoder_and_returns_interrupted() {
        let cancellation = CancellationToken::default();
        let trigger = cancellation.clone();
        let task = tokio::spawn(async move {
            ZstdCompressor::default()
                .compress_with_cancellation(
                    PendingReader,
                    SharedWriter::default(),
                    Arc::new(NoCompressionProgress),
                    cancellation,
                )
                .await
        });

        trigger.cancel();

        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("compression must stop promptly")
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, CompressionError::Interrupted));
    }

    #[test]
    fn counting_hash_writer_hashes_only_bytes_accepted_by_the_writer() {
        let mut writer = CountingHashWriter::new(PartialWriter);
        assert_eq!(writer.write(b"abcdef").unwrap(), 3);
        assert_eq!(writer.write(b"def").unwrap(), 3);
        let (bytes, checksum) = writer.finish();

        assert_eq!(bytes, 6);
        assert_eq!(
            checksum.to_string(),
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721"
        );
    }

    #[test]
    fn rates_and_ratios_are_defined_for_zero_duration_or_empty_input() {
        assert_eq!(
            CompressionProgress {
                input_bytes: 42,
                elapsed: Duration::ZERO,
            }
            .input_bytes_per_second(),
            0.0
        );
        assert_eq!(
            CompressionMetrics {
                input_bytes: 0,
                compressed_bytes: 0,
                input_sha256: Sha256Digest::from_bytes([0; 32]),
                compressed_sha256: Sha256Digest::from_bytes([0; 32]),
                elapsed: Duration::ZERO,
            }
            .compression_ratio(),
            0.0
        );
    }

    struct GeneratedReader {
        remaining: usize,
        max_requested: Arc<Mutex<usize>>,
    }

    struct FailingReader {
        emitted: bool,
    }

    struct PendingReader;

    impl AsyncRead for PendingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.emitted {
                return Poll::Ready(Err(io::Error::other("synthetic input failure")));
            }
            self.emitted = true;
            buffer.put_slice(b"partial input");
            Poll::Ready(Ok(()))
        }
    }

    impl GeneratedReader {
        fn new(remaining: usize) -> Self {
            Self {
                remaining,
                max_requested: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl AsyncRead for GeneratedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut max_requested = self.max_requested.lock().unwrap();
            *max_requested = (*max_requested).max(buffer.remaining());
            drop(max_requested);
            let bytes = self.remaining.min(buffer.remaining());
            buffer.initialize_unfilled_to(bytes).fill(b'x');
            buffer.advance(bytes);
            self.remaining -= bytes;
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Default)]
    struct SharedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter {
        remaining: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "synthetic full disk",
                ));
            }
            let written = self.remaining.min(buffer.len());
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct PartialWriter;

    impl Write for PartialWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len().min(3))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
