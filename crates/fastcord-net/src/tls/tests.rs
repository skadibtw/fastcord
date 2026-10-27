use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn handshake(host: &[u8]) -> Vec<u8> {
    let mut extensions = Vec::new();
    // A structurally similar byte sequence in another extension is ignored.
    extensions.extend_from_slice(&[0xfa, 0xfa, 0, 7, 0, 0, 0, 3, b'x', b'y', b'z']);
    let mut sni = Vec::new();
    sni.extend_from_slice(&u16::try_from(host.len() + 3).unwrap().to_be_bytes());
    sni.push(0);
    sni.extend_from_slice(&u16::try_from(host.len()).unwrap().to_be_bytes());
    sni.extend_from_slice(host);
    extensions.extend_from_slice(&[0, 0]);
    extensions.extend_from_slice(&u16::try_from(sni.len()).unwrap().to_be_bytes());
    extensions.extend_from_slice(&sni);
    // ALPN and a fake opaque PSK extension are byte-preservation canaries.
    extensions.extend_from_slice(&[0, 16, 0, 5, 0, 3, 2, b'h', b'2']);
    extensions.extend_from_slice(&[0, 41, 0, 4, 0xb1, 0x7d, 0xe2, 0x89]);
    let mut hello = vec![1, 0, 0, 0, 3, 3];
    hello.extend_from_slice(&[0x42; 32]);
    hello.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
    hello.extend_from_slice(&u16::try_from(extensions.len()).unwrap().to_be_bytes());
    hello.extend_from_slice(&extensions);
    let length = hello.len() - 4;
    hello[1..4].copy_from_slice(&[
        u8::try_from(length >> 16).unwrap(),
        u8::try_from((length >> 8) & 0xff).unwrap(),
        u8::try_from(length & 0xff).unwrap(),
    ]);
    hello
}

fn record(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![22, 3, 3];
    bytes.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn reassemble(records: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut offset = 0;
    while offset < records.len() {
        let length = u16_at(records, offset + 3).unwrap();
        bytes.extend_from_slice(&records[offset + 5..offset + 5 + length]);
        offset += 5 + length;
    }
    bytes
}

fn parsed(bytes: &[u8]) -> ClientHello {
    let Inspection::ClientHello(hello) = inspect_client_hello(bytes).unwrap() else {
        panic!("expected a complete hello");
    };
    hello
}

struct PlanControl;
impl PrefixControl for PlanControl {
    fn close(&self) {}
}

#[test]
fn actual_rustls_http_client_hello_has_a_valid_fake_plan() {
    use crate::selector::TlsProfile;
    use tokio_rustls::rustls::{ClientConnection, pki_types::ServerName};
    let mut connection = ClientConnection::new(
        crate::broker::tls_config(TlsProfile::Http, true).unwrap(),
        ServerName::try_from("discord.com").unwrap(),
    )
    .unwrap();
    let mut bytes = Vec::new();
    connection.write_tls(&mut bytes).unwrap();
    let info = parsed(&bytes);
    assert_eq!(
        &bytes[info.split_at - 1..info.split_at - 1 + 11],
        b"discord.com"
    );
    let (io, _) = ShortIo::new(usize::MAX);
    let mut stream =
        ShapedStream::new(io, Strategy::PacketFake).with_control(Arc::new(PlanControl));
    stream.bytes = bytes;
    assert!(
        stream.prepare().is_ok(),
        "prefix {}, record {:?}, sni {}",
        info.prefix_end,
        u16_at(&stream.bytes, 3),
        info.split_at
    );
    assert_eq!(stream.fake_data.len(), info.prefix_end);
    assert!(stream.fake_data.windows(11).any(|w| w == b"example.com"));
}

#[test]
fn exact_sni_position_ignores_opaque_extensions_and_preserves_alpn_and_binders() {
    let original = handshake(b"gateway.discord.gg");
    let mut bytes = record(&original);
    let hello = parsed(&bytes);
    assert_eq!(bytes[hello.split_at - 1], b'g');
    assert_eq!(bytes[hello.split_at], b'a');
    split_record(&mut bytes, hello).unwrap();
    assert_eq!(reassemble(&bytes), original);
    assert_eq!(bytes[hello.split_at..hello.split_at + 3], [22, 3, 3]);
}

#[test]
fn every_handshake_record_boundary_and_every_input_prefix() {
    let original = handshake(b"gateway.discord.gg");
    for boundary in 1..original.len() {
        let mut bytes = record(&original[..boundary]);
        bytes.extend(record(&original[boundary..]));
        for end in 0..bytes.len() {
            assert_eq!(
                inspect_client_hello(&bytes[..end]),
                Ok(Inspection::Incomplete),
                "boundary {boundary}, prefix {end}"
            );
        }
        let hello = parsed(&bytes);
        assert_eq!(bytes[hello.split_at - 1], b'g');
        split_record(&mut bytes, hello).unwrap();
        assert_eq!(reassemble(&bytes), original);
    }
}

#[test]
fn one_byte_records_existing_boundary_and_coalesced_messages() {
    let original = handshake(b"example.test");
    let mut bytes: Vec<u8> = original.chunks(1).flat_map(record).collect();
    let hello = parsed(&bytes);
    assert_eq!(hello.split_at, hello.record_end);
    let before = bytes.clone();
    split_record(&mut bytes, hello).unwrap();
    assert_eq!(bytes, before);
    assert_eq!(reassemble(&bytes), original);

    let mut coalesced = original.clone();
    coalesced.extend_from_slice(&[11, 0, 0, 0]);
    let mut bytes = record(&coalesced);
    let hello = parsed(&bytes);
    split_record(&mut bytes, hello).unwrap();
    assert_eq!(reassemble(&bytes), coalesced);
}

#[test]
fn malformed_lengths_and_oversized_inputs_have_categorical_errors() {
    let original = record(&handshake(b"example.test"));
    for index in [0, 1, 2] {
        let mut bytes = original.clone();
        bytes[index] = 0xff;
        assert_eq!(inspect_client_hello(&bytes), Err(ParseError::InvalidRecord));
    }
    let mut oversized = record(&[1, 1, 0, 0]);
    assert_eq!(
        inspect_client_hello(&oversized),
        Err(ParseError::HelloTooLarge)
    );
    oversized = vec![0; MAX_RAW + 1];
    assert_eq!(
        inspect_client_hello(&oversized),
        Err(ParseError::StagingLimit)
    );
    let mut bytes = original;
    // The top-level extensions size must cover exactly the handshake.
    bytes[5 + 45] = 0xff;
    assert_eq!(
        inspect_client_hello(&bytes),
        Err(ParseError::InvalidHandshake)
    );
    assert_eq!(
        format!("{:?}", ParseError::InvalidHandshake),
        "InvalidHandshake"
    );
}

#[test]
fn malformed_sni_and_duplicate_extensions_are_rejected() {
    let hello = handshake(b"example.test");
    let bytes = record(&hello);
    let sni = parsed(&bytes).split_at - 1;
    for field in [sni - 1, sni - 2, sni - 4, sni - 5] {
        let mut corrupt = bytes.clone();
        corrupt[field] = 0xff;
        assert!(inspect_client_hello(&corrupt).is_err());
    }
    let mut empty = handshake(b"");
    assert_eq!(
        inspect_client_hello(&record(&empty)),
        Err(ParseError::InvalidSni)
    );
    // Safe handling of every arbitrary truncation, including extension headers.
    for end in 0..hello.len() {
        empty = hello[..end].to_vec();
        let _ = inspect_client_hello(&record(&empty));
    }
    let host_start = parsed(&bytes).split_at - 6;
    let extension_start = host_start - 9;
    let duplicate = hello[extension_start..host_start + b"example.test".len()].to_vec();
    let mut duplicate_hello = hello;
    duplicate_hello.extend_from_slice(&duplicate);
    let extensions_len = u16_at(&duplicate_hello, 45).unwrap() + duplicate.len();
    duplicate_hello[45..47].copy_from_slice(&u16::try_from(extensions_len).unwrap().to_be_bytes());
    let length = u32::try_from(duplicate_hello.len() - 4)
        .unwrap()
        .to_be_bytes();
    duplicate_hello[1..4].copy_from_slice(&length[1..]);
    assert_eq!(
        inspect_client_hello(&record(&duplicate_hello)),
        Err(ParseError::InvalidSni)
    );
}

#[test]
fn maximum_hello_and_deterministic_mutations_stay_bounded() {
    let mut hello = handshake(b"example.test");
    let padding = MAX_CLIENT_HELLO - hello.len();
    hello.extend_from_slice(&[0, 21]);
    hello.extend_from_slice(&u16::try_from(padding - 4).unwrap().to_be_bytes());
    hello.resize(MAX_CLIENT_HELLO, 0);
    let extensions_len = hello.len() - 47;
    hello[45..47].copy_from_slice(&u16::try_from(extensions_len).unwrap().to_be_bytes());
    let length = u32::try_from(hello.len() - 4).unwrap().to_be_bytes();
    hello[1..4].copy_from_slice(&length[1..]);
    let mut bytes: Vec<_> = hello.chunks(MAX_RECORD).flat_map(record).collect();
    let info = parsed(&bytes);
    split_record(&mut bytes, info).unwrap();
    assert_eq!(reassemble(&bytes), hello);

    let good = record(&handshake(b"example.test"));
    for offset in 0..good.len() {
        for mutation in [0, 1, 0x7f, 0xff] {
            let mut bytes = good.clone();
            bytes[offset] = mutation;
            // No panics or allocations proportional to declared lengths.
            if let Ok(Inspection::ClientHello(info)) = inspect_client_hello(&bytes) {
                let original = reassemble(&bytes);
                split_record(&mut bytes, info).unwrap();
                assert_eq!(reassemble(&bytes), original);
            }
        }
    }
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    writes: Vec<(tokio::time::Instant, usize)>,
}

struct ShortIo {
    output: Arc<Mutex<Output>>,
    max_write: usize,
    pending: bool,
}

impl ShortIo {
    fn new(max_write: usize) -> (Self, Arc<Mutex<Output>>) {
        let output = Arc::new(Mutex::new(Output::default()));
        (
            Self {
                output: output.clone(),
                max_write,
                pending: false,
            },
            output,
        )
    }
}

impl AsyncWrite for ShortIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.pending = !this.pending;
        if this.pending {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let count = buffer.len().min(this.max_write);
        let mut output = this.output.lock().unwrap();
        let position = output.bytes.len();
        output.writes.push((tokio::time::Instant::now(), position));
        output.bytes.extend_from_slice(&buffer[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for ShortIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        buffer.put_slice(b"r");
        Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn every_write_boundary_short_writes_and_exact_accepted_byte_accounting() {
    let original = handshake(b"gateway.discord.gg");
    let bytes = record(&original);
    for strategy in Strategy::LEGACY {
        for boundary in 1..bytes.len() {
            let (io, output) = ShortIo::new(3);
            let mut stream = ShapedStream::new(io, strategy);
            stream.write_all(&bytes[..boundary]).await.unwrap();
            stream.write_all(&bytes[boundary..]).await.unwrap();
            stream.flush().await.unwrap();
            assert_eq!(reassemble(&output.lock().unwrap().bytes), original);
            assert_eq!(stream.bytes.capacity(), 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn record_fragment_keeps_writes_off_every_record_boundary() {
    let original = handshake(b"gateway.discord.gg");
    let bytes = record(&original);
    let mut refragmented = bytes.clone();
    let info = parsed(&refragmented);
    let boundaries = fragment_record(&mut refragmented, info).unwrap();
    assert_eq!(boundaries.len(), FRAGMENTS);
    assert_eq!(reassemble(&refragmented), original);
    assert_eq!(refragmented.len(), bytes.len() + 5 * (FRAGMENTS - 1));
    for boundary in &boundaries[..FRAGMENTS - 1] {
        assert_eq!(refragmented[*boundary..*boundary + 3], [22, 3, 3]);
    }
    assert_eq!(boundaries[FRAGMENTS - 1], refragmented.len());
    let (io, output) = ShortIo::new(usize::MAX);
    let mut stream = ShapedStream::new(io, Strategy::RecordFragment);
    let started = tokio::time::Instant::now();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
    let output = output.lock().unwrap();
    assert_eq!(reassemble(&output.bytes), original);
    assert_eq!(output.bytes.len(), refragmented.len());
    // One write per fragment, each starting one byte before a record boundary,
    // so no segment beginning is a record header.
    let interior: Vec<usize> = boundaries[..FRAGMENTS - 1].iter().map(|b| b - 1).collect();
    assert_eq!(output.writes.len(), FRAGMENTS);
    assert_eq!(output.writes[0], (started, 0));
    for (index, start) in interior.iter().enumerate() {
        assert_eq!(
            output.writes[index + 1],
            (
                started + WRITE_DELAY * u32::try_from(index + 1).unwrap(),
                *start
            )
        );
        assert!(!boundaries.contains(start));
    }
    assert_eq!(stream.bytes.capacity(), 0);
}

#[tokio::test(start_paused = true)]
async fn record_fragment_preserves_a_hello_that_already_spans_records() {
    let original = handshake(b"gateway.discord.gg");
    let mut bytes = record(&original[..20]);
    bytes.extend(record(&original[20..]));
    let (io, output) = ShortIo::new(4);
    let mut stream = ShapedStream::new(io, Strategy::RecordFragment);
    for chunk in bytes.chunks(5) {
        stream.write_all(chunk).await.unwrap();
    }
    stream.flush().await.unwrap();
    assert_eq!(reassemble(&output.lock().unwrap().bytes), original);
    assert_eq!(stream.bytes.capacity(), 0);
}

#[tokio::test(start_paused = true)]
async fn opaque_bridge_drains_only_complete_prefix_across_arbitrary_network_reads() {
    let original = handshake(b"gateway.discord.gg");
    let bytes = record(&original);
    for strategy in [
        Strategy::TlsRecordSplit,
        Strategy::WriteSplit,
        Strategy::Combined,
    ] {
        for chunk_size in 1..bytes.len() {
            let (io, output) = ShortIo::new(3);
            let mut stream = ShapedStream::new(io, strategy);
            let mut accepted = 0;
            for chunk in bytes.chunks(chunk_size) {
                stream.write_all(chunk).await.unwrap();
                accepted += chunk.len();
                stream.drain_ready().await.unwrap();
                if accepted < bytes.len() {
                    assert!(output.lock().unwrap().bytes.is_empty());
                }
            }
            let output = output.lock().unwrap();
            assert_eq!(reassemble(&output.bytes), original);
            if strategy.record_split() {
                assert_eq!(output.bytes.len(), bytes.len() + 5);
            }
            assert_eq!(stream.bytes.capacity(), 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn write_split_has_one_async_delay_at_the_structural_position() {
    let bytes = record(&handshake(b"gateway.discord.gg"));
    let split = parsed(&bytes).split_at;
    for strategy in [Strategy::WriteSplit, Strategy::Combined] {
        let (io, output) = ShortIo::new(usize::MAX);
        let mut stream = ShapedStream::new(io, strategy);
        let started = tokio::time::Instant::now();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
        let output = output.lock().unwrap();
        assert_eq!(output.writes.len(), 2);
        assert_eq!(output.writes[0], (started, 0));
        assert_eq!(output.writes[1], (started + WRITE_DELAY, split));
    }
}

#[tokio::test(start_paused = true)]
async fn hello_retry_request_compatibility_ccs_then_protected_data() {
    let hello = handshake(b"example.test");
    for strategy in Strategy::LEGACY {
        let (io, output) = ShortIo::new(7);
        let mut stream = ShapedStream::new(io, strategy);
        let first = record(&hello);
        stream.write_all(&first).await.unwrap();
        stream.flush().await.unwrap();
        let mut hrr = vec![2, 0, 0, 34, 3, 3];
        hrr.extend_from_slice(&HRR_RANDOM);
        stream.server_hello_observer().observe(&record(&hrr));
        let mut second = vec![20, 3, 3, 0, 1, 1];
        second.extend(record(&hello));
        second.extend_from_slice(&[23, 3, 3, 0, 4, 0xab, 0xcd, 0xef, 0]);
        stream.write_all(&second).await.unwrap();
        stream.flush().await.unwrap();
        let mut expected = hello.clone();
        expected.extend_from_slice(&[1]); // CCS body.
        expected.extend_from_slice(&hello);
        expected.extend_from_slice(&[0xab, 0xcd, 0xef, 0]);
        assert_eq!(reassemble(&output.lock().unwrap().bytes), expected);
        assert_eq!(stream.bytes.capacity(), 0);
        assert_eq!(
            stream.hellos,
            if strategy == Strategy::Direct { 0 } else { 2 }
        );
        let suffix = [22, 3, 3, 0, 1, 0xff];
        stream.write_all(&suffix).await.unwrap();
        stream.flush().await.unwrap();
        assert!(output.lock().unwrap().bytes.ends_with(&suffix));
    }
}

#[tokio::test]
async fn tls12_ccs_protects_even_finished_ciphertext_that_resembles_a_valid_hello() {
    let hello = record(&handshake(b"example.test"));
    for strategy in [
        Strategy::TlsRecordSplit,
        Strategy::WriteSplit,
        Strategy::Combined,
    ] {
        let (io, output) = ShortIo::new(64);
        let mut stream = ShapedStream::new(io, strategy);
        stream.write_all(&hello).await.unwrap();
        stream.flush().await.unwrap();
        let prefix_len = output.lock().unwrap().bytes.len();
        let mut protected = vec![20, 3, 3, 0, 1, 1];
        // Deliberately indistinguishable ciphertext bytes; a protected record
        // must pass byte-for-byte, without relying on random ciphertext odds.
        protected.extend_from_slice(&hello);
        for chunk in protected.chunks(1) {
            stream.write_all(chunk).await.unwrap();
            stream.drain_ready().await.unwrap();
        }
        stream.flush().await.unwrap();
        assert_eq!(output.lock().unwrap().bytes[prefix_len..], protected);
        assert_eq!(stream.hellos, 1);
    }
}

#[test]
fn incoming_hrr_observation_handles_every_record_and_read_boundary() {
    let mut hrr = vec![2, 0, 0, 34, 3, 3];
    hrr.extend_from_slice(&HRR_RANDOM);
    for record_split in 1..hrr.len() {
        let mut bytes = record(&hrr[..record_split]);
        bytes.extend(record(&hrr[record_split..]));
        for read_split in 0..=bytes.len() {
            let mut observer = ServerHelloObserver::new(true);
            let flag = observer.clone();
            observer.observe(&bytes[..read_split]);
            observer.observe(&bytes[read_split..]);
            assert!(flag.retry_hello_allowed());
        }
    }
    let mut normal = hrr;
    normal[6] ^= 1;
    let mut observer = ServerHelloObserver::new(true);
    observer.observe(&record(&normal));
    assert!(!observer.retry_hello_allowed());
}

#[tokio::test(start_paused = true)]
async fn incomplete_flush_and_read_make_progress_without_replaying_a_prefix() {
    let bytes = record(&handshake(b"example.test"));
    for read in [false, true] {
        let (io, output) = ShortIo::new(3);
        let mut stream = ShapedStream::new(io, Strategy::Combined);
        stream.write_all(&bytes[..7]).await.unwrap();
        if read {
            let mut answer = [0];
            stream.read_exact(&mut answer).await.unwrap();
            assert_eq!(answer, [b'r']);
        } else {
            stream.flush().await.unwrap();
        }
        stream.write_all(&bytes[7..]).await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(output.lock().unwrap().bytes, bytes);
    }
    let (io, output) = ShortIo::new(3);
    let mut stream = ShapedStream::new(io, Strategy::Combined);
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
    let already_sent = output.lock().unwrap().bytes.clone();
    stream.write_all(&bytes[..7]).await.unwrap();
    assert_eq!(
        stream.flush().await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(output.lock().unwrap().bytes, already_sent);
    assert!(stream.write_all(&bytes).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_delay_resumes_once_and_shutdown_drains() {
    let bytes = record(&handshake(b"example.test"));
    let (io, output) = ShortIo::new(usize::MAX);
    let mut stream = ShapedStream::new(io, Strategy::Combined);
    stream.write_all(&bytes).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), stream.flush())
            .await
            .is_err()
    );
    stream.shutdown().await.unwrap();
    assert_eq!(
        reassemble(&output.lock().unwrap().bytes),
        handshake(b"example.test")
    );
    assert_eq!(stream.bytes.capacity(), 0);
}

#[tokio::test]
async fn malformed_input_passes_only_before_any_transformation() {
    let bad = [22, 3, 3, 0, 4, 1, 1, 0, 0];
    let (io, output) = ShortIo::new(64);
    let mut stream = ShapedStream::new(io, Strategy::TlsRecordSplit);
    stream.write_all(&bad).await.unwrap();
    stream.flush().await.unwrap();
    assert_eq!(output.lock().unwrap().bytes, bad);
    let (io, output) = ShortIo::new(64);
    let mut stream = ShapedStream::new(io, Strategy::TlsRecordSplit);
    stream
        .write_all(&record(&handshake(b"example.test")))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let before = output.lock().unwrap().bytes.clone();
    assert!(stream.write_all(&bad).await.is_err());
    assert_eq!(output.lock().unwrap().bytes, before);
}

#[tokio::test]
async fn real_rustls_tls12_tls13_alpn_hrr_and_application_data() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, HandshakeKind, RootCertStore, ServerConfig, crypto};
    use tokio_rustls::rustls;

    // These are public localhost-only test fixtures, never production roots.
    let certificate = CertificateDer::from(include_bytes!("fixtures/test-cert.der").to_vec());
    let key = include_bytes!("fixtures/test-key.der");
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        for hrr in [false, true] {
            if version == &rustls::version::TLS12 && hrr {
                continue;
            }
            for strategy in Strategy::LEGACY {
                let mut roots = RootCertStore::empty();
                roots.add(certificate.clone()).unwrap();
                let mut client =
                    ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                        .with_protocol_versions(&[version])
                        .unwrap()
                        .with_root_certificates(roots)
                        .with_no_client_auth();
                client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                let mut provider = crypto::ring::default_provider();
                if hrr {
                    provider.kx_groups = vec![crypto::ring::kx_group::SECP256R1];
                }
                let mut server = ServerConfig::builder_with_provider(Arc::new(provider))
                    .with_protocol_versions(&[version])
                    .unwrap()
                    .with_no_client_auth()
                    .with_single_cert(
                        vec![certificate.clone()],
                        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
                    )
                    .unwrap();
                server.alpn_protocols = vec![b"h2".to_vec()];
                // Tiny duplex capacity exercises read/write backpressure and
                // forces the handshake to make progress through partial I/O.
                let (client_io, server_io) = tokio::io::duplex(31);
                let budget = Arc::new(Semaphore::new(4));
                let client_io = ShapedStream::with_budget(client_io, strategy, budget.clone());
                let client_side = async {
                    let mut stream = tokio_rustls::TlsConnector::from(Arc::new(client))
                        .connect(ServerName::try_from("localhost").unwrap(), client_io)
                        .await
                        .unwrap();
                    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                    if hrr {
                        assert_eq!(
                            stream.get_ref().1.handshake_kind(),
                            Some(HandshakeKind::FullWithHelloRetryRequest)
                        );
                    }
                    stream.write_all(b"application data").await.unwrap();
                    stream.flush().await.unwrap();
                    let mut answer = [0; 5];
                    stream.read_exact(&mut answer).await.unwrap();
                    assert_eq!(&answer, b"reply");
                    assert_eq!(stream.get_ref().0.bytes.capacity(), 0);
                };
                let server_side = async {
                    let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(server))
                        .accept(server_io)
                        .await
                        .unwrap();
                    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                    assert_eq!(stream.get_ref().1.server_name(), Some("localhost"));
                    let mut input = [0; 16];
                    stream.read_exact(&mut input).await.unwrap();
                    assert_eq!(&input, b"application data");
                    stream.write_all(b"reply").await.unwrap();
                    stream.flush().await.unwrap();
                };
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(client_side, server_side)
                })
                .await
                .unwrap_or_else(|_| panic!("deadlock: {version:?}, HRR {hrr}, {strategy:?}"));
                assert_eq!(budget.available_permits(), 4);
            }
        }
    }
}

#[tokio::test]
async fn staging_budget_is_acquired_before_allocation_and_released_after_prefix() {
    let budget = Arc::new(Semaphore::new(0));
    let bytes = record(&handshake(b"example.test"));
    let (io, output) = ShortIo::new(64);
    let mut stream = ShapedStream::with_budget(io, Strategy::TlsRecordSplit, budget.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), stream.write_all(&bytes))
            .await
            .is_err()
    );
    assert_eq!(stream.bytes.capacity(), 0);
    assert!(output.lock().unwrap().bytes.is_empty());
    budget.add_permits(1);
    stream.write_all(&bytes).await.unwrap();
    assert_eq!(budget.available_permits(), 0);
    assert!(stream.bytes.capacity() <= MAX_RAW);
    stream.flush().await.unwrap();
    assert_eq!(stream.bytes.capacity(), 0);
    assert_eq!(budget.available_permits(), 1);
}

#[tokio::test]
async fn real_tls12_abbreviated_session_resumption_preserves_encrypted_finished() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, HandshakeKind, RootCertStore, ServerConfig, crypto};
    use tokio_rustls::rustls;

    let certificate = CertificateDer::from(include_bytes!("fixtures/test-cert.der").to_vec());
    for strategy in Strategy::LEGACY {
        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let client = Arc::new(
            ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            include_bytes!("fixtures/test-key.der").to_vec(),
        ));
        let server = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], key)
                .unwrap(),
        );
        for kind in [HandshakeKind::Full, HandshakeKind::Resumed] {
            let (client_io, server_io) = tokio::io::duplex(31);
            let client_io = ShapedStream::new(client_io, strategy);
            let client_side = async {
                let mut stream = tokio_rustls::TlsConnector::from(client.clone())
                    .connect(ServerName::try_from("localhost").unwrap(), client_io)
                    .await
                    .unwrap();
                assert_eq!(stream.get_ref().1.handshake_kind(), Some(kind));
                stream.write_all(b"x").await.unwrap();
                stream.flush().await.unwrap();
                let mut answer = [0];
                stream.read_exact(&mut answer).await.unwrap();
                assert_eq!(answer, [b'y']);
            };
            let server_side = async {
                let mut stream = tokio_rustls::TlsAcceptor::from(server.clone())
                    .accept(server_io)
                    .await
                    .unwrap();
                let mut input = [0];
                stream.read_exact(&mut input).await.unwrap();
                assert_eq!(input, [b'x']);
                stream.write_all(b"y").await.unwrap();
                stream.flush().await.unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(client_side, server_side)
            })
            .await
            .unwrap();
        }
    }
}

fn be16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes([b[at], b[at + 1]]))
}

/// (cipher suite bytes, extension types in order, total length) of a hello.
fn hello_shape(bytes: &[u8]) -> (Vec<u8>, Vec<usize>, usize) {
    let mut at = 5 + 4 + 2 + 32;
    at += 1 + usize::from(bytes[at]);
    let suites = be16(bytes, at);
    let cipher_suites = bytes[at + 2..at + 2 + suites].to_vec();
    at += 2 + suites;
    at += 1 + usize::from(bytes[at]);
    let end = at + 2 + be16(bytes, at);
    at += 2;
    let mut types = Vec::new();
    while at < end {
        types.push(be16(bytes, at));
        at += 4 + be16(bytes, at + 2);
    }
    (cipher_suites, types, bytes.len())
}

/// The OS verifier must not change the ClientHello (#131): the extension
/// order, cipher suites and length equal those of a config with the embedded
/// webpki roots, which is what shipped before.
#[test]
fn platform_verifier_leaves_the_client_hello_unchanged() {
    use crate::selector::TlsProfile;
    use tokio_rustls::rustls::{
        ClientConfig, ClientConnection, RootCertStore, crypto, pki_types::ServerName,
    };
    let hello = |config: Arc<ClientConfig>| {
        let mut connection =
            ClientConnection::new(config, ServerName::try_from("discord.com").unwrap()).unwrap();
        let mut bytes = Vec::new();
        connection.write_tls(&mut bytes).unwrap();
        let (suites, mut types, length) = hello_shape(&bytes);
        // rustls shuffles the order per connection, so compare the set.
        types.sort_unstable();
        (suites, types, length)
    };
    let mut old = ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        ))
        .with_no_client_auth();
    old.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    old.resumption = tokio_rustls::rustls::client::Resumption::disabled();
    old.enable_early_data = false;
    let new = crate::broker::tls_config(TlsProfile::Http, true).unwrap();
    assert_eq!(hello(Arc::new(old)), hello(new));
}

/// A server with a self-signed certificate nobody trusts is rejected by the
/// production config, offline.
#[tokio::test]
async fn production_config_rejects_an_untrusted_self_signed_server() {
    use crate::selector::TlsProfile;
    use tokio_rustls::rustls::{
        ServerConfig, crypto,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    };
    let certificate = CertificateDer::from(include_bytes!("fixtures/test-cert.der").to_vec());
    let server = ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("fixtures/test-key.der").to_vec(),
            )),
        )
        .unwrap();
    let (client_io, server_io) = tokio::io::duplex(16 * 1024);
    let client = tokio_rustls::TlsConnector::from(
        crate::broker::tls_config(TlsProfile::WebSocket, false).unwrap(),
    )
    .connect(ServerName::try_from("localhost").unwrap(), client_io);
    let server = tokio_rustls::TlsAcceptor::from(Arc::new(server)).accept(server_io);
    let (client, _) = tokio::join!(client, server);
    assert!(client.is_err());
}
