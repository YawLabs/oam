//! Local servers, a fake proxy and the TLS fixtures the http_client transport
//! and fetch tests share. Everything listens on 127.0.0.1 and speaks HTTP by
//! hand, so a test sees the exact bytes a request put on the wire.

#![allow(dead_code)]

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use http::header::HeaderValue;
use oam_core::http_client::tls_config::TlsConfigs;
use oam_core::http_client::{HttpTransport, ProxySource, TlsSource, TransportOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A test body that hangs fails in this long rather than wedging the suite.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn within<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, fut)
        .await
        .expect("test timed out")
}

pub const USER_AGENT: &str = "oam/0.0.0-test";

/// rustls needs a process-wide provider for the configs the tests build with
/// `builder_with_provider` only where an API reads the default.
pub fn install_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A port nothing listens on (bound, then released).
pub async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A transport that trusts the test CA only.
pub fn transport(proxy: ProxySource) -> HttpTransport {
    transport_with_tls(TlsSource::Fixed(test_tls()), proxy)
}

pub fn transport_with_tls(tls: TlsSource, proxy: ProxySource) -> HttpTransport {
    HttpTransport::with_options(TransportOptions {
        tls,
        proxy,
        user_agent: HeaderValue::from_static(USER_AGENT),
    })
}

// ---------------------------------------------------------------- TLS

/// A throwaway private CA (CA:TRUE, valid to 2126) and the leaf it signed
/// (SAN DNS:localhost, IP:127.0.0.1) -- the fixtures of
/// crates/oam_cli/tests/e2e.rs, copied verbatim.
pub const TLS_TEST_CA_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDHzCCAgegAwIBAgIUX5ir308lg8m4hQdNnUz0UdN33DwwDQYJKoZIhvcNAQEL\n\
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI0WhgPMjEy\n\
NjA4MjExMTExMjRaMBYxFDASBgNVBAMMC29hbSB0ZXN0IENBMIIBIjANBgkqhkiG\n\
9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5oXf7XNg5MHjC511VA64HF8kdBHebuI207US\n\
fCQg9EYTe3hzOBACwsn78SNXFfmDw5E7hlF2xTuZmD3OJx9a0Ax54EoF67Z4Bigw\n\
My6GF1oKNsmeCGn9nv62+7jm9UspForbmWE8/rC3bM37BbvS87FoogEdXQS5uNQz\n\
4AuGbduhr27IXlScHsub4paSIrW6etllby5Ja+81NpVmwuZ32QNk+s0bwcLq8YIq\n\
5zpemaKeTGDBbG3mIt3vYsfjg8zUTdCdkjOs8q0+BSB8OkhGpe888d5JyUxd1WiK\n\
qiTpfG3+2Pbr0eK7pzIzeT+HDfzUInFfr7lu6lBtfjQRWSZWGwIDAQABo2MwYTAd\n\
BgNVHQ4EFgQUKmakijzWE71HQeyNAwaTnq9/xyMwHwYDVR0jBBgwFoAUKmakijzW\n\
E71HQeyNAwaTnq9/xyMwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw\n\
DQYJKoZIhvcNAQELBQADggEBACX/zgcVyya26/+5t6Be9duAAJs1X0VSKSzXP/Au\n\
A+ngqWqFBPDhIzorx84d+siuRKVLOZUjObba245P4oiaJwNSz3Ihix5V3FHGZTVM\n\
vHpVP8V7tzKpoEz89vfhueFOB0u2TVJe/099DAHrjaaza0zWa1zfxucrBAFQiQIA\n\
2GK95UN3sSv9/rl3QlxQx8ld5QlpIjjhQL7N1JWWKcuBqDHgbfN1qwB2CWSB+v3g\n\
YyTiYg/yyeFi173xPPS3CoiyyVyO+6ySfhwvopDJVkTdZafDpV5/d1o+AssKYX3R\n\
Jd1U3J1YgXh3HzZEI9Yeo2jZzDogNzNObNoYdXRUoDVPMXo=\n\
-----END CERTIFICATE-----";

pub const TLS_TEST_LEAF_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDRzCCAi+gAwIBAgIUXMdiPT0RoKd1ynyNQq5kRcwrF9UwDQYJKoZIhvcNAQEL\n\
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI1WhgPMjEy\n\
NjA4MjExMTExMjVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcN\n\
AQEBBQADggEPADCCAQoCggEBALtUgW8legRgDaIObCQ75gb63jPvvGLkgrmfvL+z\n\
zuIpFr6McD3Em6aX0fje4x8SjVF10F1HTa8pLDy4G6T/UiBuATovjMsEIqk1MLW2\n\
F6/KfQLO35pVC6PeUCYW8UkqymVifxsPQuzdV+Hbp9VDaamHtCFhJN0sl0TAbc37\n\
xp4WZwI1HTSQ4q+ReLSslNQiK+bwJQeKdiL7u6jzXqkb0uTxOJ2bSS2BhpPbPiNR\n\
fZObJiFr6wtURUvy0AY9AmbNJwuWkuM0aJlOibaVIPPgVGDtZJCd8gQEdV4pKIMZ\n\
avTN3AbNeIMmn3nZehk5jvEHxL+tjTXG8no5f5X2KFlMwi0CAwEAAaOBjDCBiTAa\n\
BgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMC\n\
BaAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFJpXOwzKMtLLnbaIViTA\n\
QsTBV5+8MB8GA1UdIwQYMBaAFCpmpIo81hO9R0HsjQMGk56vf8cjMA0GCSqGSIb3\n\
DQEBCwUAA4IBAQCsP5gsrw1RHvEN9oBR1Pf+CXylfpH7It7ZMWDFW73rdhuC3Zxr\n\
22zgG04mRt2Gd4Ufq4FCjqELVoecWx5U/hv2v/4KmVqegJkcTnMOmQ3Bs391XXa9\n\
C+07yxnaDXE19agNm4ZACwmdf30LPaSqeVp3Y3aw8lH+5KeWrrVBpi7m8NMyHThC\n\
Yn0a/DcxRET01zHZb6AEve5eJT6Lm0YF/DF6r4+YfGehLX892VDoWgrNCz7DpDuC\n\
1ALfON7I9FSAJGh3iBvTbX9R7xVuKd8Za2f8Xwr/t7jK/zYxLAT9oyTH1FXIFAnP\n\
H5shelNOFfKjeO2TTJ9u7hMSzF9fWd4EOB7u\n\
-----END CERTIFICATE-----";

pub const TLS_TEST_LEAF_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC7VIFvJXoEYA2i\n\
DmwkO+YG+t4z77xi5IK5n7y/s87iKRa+jHA9xJuml9H43uMfEo1RddBdR02vKSw8\n\
uBuk/1IgbgE6L4zLBCKpNTC1thevyn0Czt+aVQuj3lAmFvFJKsplYn8bD0Ls3Vfh\n\
26fVQ2mph7QhYSTdLJdEwG3N+8aeFmcCNR00kOKvkXi0rJTUIivm8CUHinYi+7uo\n\
816pG9Lk8Tidm0ktgYaT2z4jUX2TmyYha+sLVEVL8tAGPQJmzScLlpLjNGiZTom2\n\
lSDz4FRg7WSQnfIEBHVeKSiDGWr0zdwGzXiDJp952XoZOY7xB8S/rY01xvJ6OX+V\n\
9ihZTMItAgMBAAECggEADSEodpMjMNilRqJ0JJCo1/xlQ9vy/DYVONAKo/UE9Fz6\n\
Nx4TZSuOgpKe04Prr0CBnx/+xqA6FaNxHPWvxP9le4MPmvW84c3HECJQ6QDQ5YVF\n\
AG63b/2zSdJJvncFL6JMJTxODvt22VskzwkHg68B4jFHXWo4Rzgvh1C6tsvavoxS\n\
DA/J/Pl+saC6iccDtLp4lbJaMzCGGRDPjb13hqBcHoPEjF5JtN9I1bCVUZn/QFbY\n\
7PHRptS2SDuAcoPiC8SlqZff7PSMakZzBT7Ng7kSdW3mFapJkN2NM5IsmIlTyG83\n\
1GfTXCH1o00HXpoJ8N5YundxoG5FWlCIgcrEO1jNEQKBgQDbCJapOXVz6ULoTevi\n\
SzdQH38UH1Ckd1rp0QYxm/MWXXyupWnBBt2iBekbFygT2bRwhtIA33CEusYmh4sN\n\
nal6ERh5wbYYzngPaO0sHX4QVzBYleu344/pkgZxCEpG8G5oxjggj/ds15TsgLVS\n\
KEsvXnodKmVsvDqfDFWD+ZnEzwKBgQDa8ijtlbO2ro9HgvqAr88kS/8nVlnZdXE5\n\
9YT/DEYVsLQzduIze9G4uzI/dgSn8UtUatCvgREFB2CkQUvSeUEF4LIH6zhiI2eu\n\
yJzhAR3tU6hXWsJSLSMildlv6ooWngdNmQg9pXTNbUjJ4dtfn1Rip5A/FKzsLxDG\n\
/mjx6R3AQwKBgHTfA0zuXM5pY4sCsN+BVNVKyQranrPy/66NGqnz1WRUo8eoeWJG\n\
oJHoZ3ZOB9N3sYDtXzaaAra/1iUO49JzEtAQOSgWhWx9FrDaQtrsLazYaPKLpEft\n\
g4eUpB1B2Cg7+B2tzpsJVnNcIJmFH7rjxyJSXgQb8Bxx3zGoaiTOVQ8fAoGAST2G\n\
iWtxkaO1FEPxTkkBbu/pK5yMM91AghXqZnMRosHYlfqn0ncSAczFE0uEZTWncFbG\n\
9l6jdd4w6uFY3tBm+vNeOp3p35JeZa6AJBh+jVxVzNr0dA7bWP9tnC2GAejdIo0V\n\
n6GQgAOVvMrL2qHu1Y2eCCv/aIaaAycprfrAVAcCgYBF6Hs47CZ4RPMzUnlMV8F+\n\
F7McNeFuVRqpneXVSNB7UDuID2ttb7RTchZaG2hc84LWRV0/yjElLrG6yPxyGFIq\n\
hnNgLVJt6pGXwWKx6CgqUvijJFPNwDhZRYtLfyCWXHDQ4E9T3C5DO7T+8lafH6NO\n\
lAvLJ1NDDacIcdciXw6fZg==\n\
-----END PRIVATE KEY-----";

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Client configs that trust the test CA and nothing else, through the same
/// constructor the platform configs use.
pub fn test_tls() -> TlsConfigs {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut TLS_TEST_CA_CERT.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(ring())
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConfigs::from_client_config(config)
}

/// A TLS acceptor presenting the test leaf, offering `alpn`.
pub fn tls_acceptor(alpn: &[&[u8]]) -> tokio_rustls::TlsAcceptor {
    let certs = rustls_pemfile::certs(&mut TLS_TEST_LEAF_CERT.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut TLS_TEST_LEAF_KEY.as_bytes())
        .unwrap()
        .unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(ring())
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

// ---------------------------------------------------------------- raw HTTP

/// A parsed request head.
#[derive(Debug, Clone)]
pub struct Head {
    pub method: String,
    pub target: String,
    /// Names lowercased, in wire order, repeated names kept.
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// One request as a server received it.
#[derive(Debug, Clone)]
pub struct Received {
    pub head: Head,
    pub body: Vec<u8>,
}

/// A server-side connection that reads requests off the wire by hand.
pub struct Conn<S> {
    pub io: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    pub fn new(io: S) -> Conn<S> {
        Conn {
            io,
            buf: Vec::new(),
        }
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 16 * 1024];
        match self.io.read(&mut chunk).await {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    async fn line(&mut self) -> Option<String> {
        loop {
            if let Some(at) = find(&self.buf, b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..at + 2).collect();
                return Some(String::from_utf8_lossy(&line[..at]).into_owned());
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn exact(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.buf.len() < n {
            if !self.fill().await {
                return None;
            }
        }
        Some(self.buf.drain(..n).collect())
    }

    /// The next request head, or None at EOF.
    pub async fn head(&mut self) -> Option<Head> {
        let end = loop {
            if let Some(at) = find(&self.buf, b"\r\n\r\n") {
                break at;
            }
            if !self.fill().await {
                return None;
            }
        };
        let raw: Vec<u8> = self.buf.drain(..end + 4).collect();
        let text = String::from_utf8_lossy(&raw[..end]).into_owned();
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next()?.splitn(3, ' ');
        let method = request_line.next()?.to_string();
        let target = request_line.next()?.to_string();
        let headers = lines
            .filter_map(|l| {
                let (name, value) = l.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        Some(Head {
            method,
            target,
            headers,
        })
    }

    /// The body of the request `head` introduced (content-length or chunked).
    pub async fn body(&mut self, head: &Head) -> Option<Vec<u8>> {
        if head
            .get("transfer-encoding")
            .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
        {
            let mut body = Vec::new();
            loop {
                let size_line = self.line().await?;
                let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
                if size == 0 {
                    // No trailers: the terminating blank line.
                    self.line().await?;
                    return Some(body);
                }
                let chunk = self.exact(size + 2).await?;
                body.extend_from_slice(&chunk[..size]);
            }
        }
        match head.get("content-length") {
            Some(len) => self.exact(len.parse().ok()?).await,
            None => Some(Vec::new()),
        }
    }

    /// The next whole request, or None at EOF.
    pub async fn request(&mut self) -> Option<Received> {
        let head = self.head().await?;
        let body = self.body(&head).await?;
        Some(Received { head, body })
    }

    pub async fn send(&mut self, bytes: &[u8]) -> bool {
        self.io.write_all(bytes).await.is_ok() && self.io.flush().await.is_ok()
    }

    /// True if the peer closed (read returned EOF or an error) within `wait`.
    pub async fn closed_within(&mut self, wait: Duration) -> bool {
        let mut chunk = [0u8; 1024];
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.io.read(&mut chunk)).await {
                Err(_) => return false,
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => {}
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// An HTTP/1.1 response: `status` is e.g. "200 OK"; a content-length is
/// added unless `headers` names one or a transfer-encoding.
pub fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
    let framed = headers.iter().any(|(n, _)| {
        n.eq_ignore_ascii_case("content-length") || n.eq_ignore_ascii_case("transfer-encoding")
    });
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if !framed {
        out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// A server on 127.0.0.1 running `handler` for every accepted connection.
pub struct Server {
    pub port: u16,
    accepts: Arc<AtomicUsize>,
    /// Every request a handler chose to record, in arrival order.
    pub seen: Arc<Mutex<Vec<Received>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    pub fn seen(&self) -> Vec<Received> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The recorder a handler gets: push what the connection received.
pub type Seen = Arc<Mutex<Vec<Received>>>;

pub async fn serve<F, Fut>(handler: F) -> Server
where
    F: Fn(Conn<TcpStream>, usize, Seen) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let task = {
        let accepts = accepts.clone();
        let seen = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let n = accepts.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                let seen = seen.clone();
                tokio::spawn(async move { handler(Conn::new(stream), n, seen).await });
            }
        })
    };
    Server {
        port,
        accepts,
        seen,
        task,
    }
}

/// A keep-alive server answering every request with `reply(request)` and
/// recording it.
pub async fn serve_replies<F>(reply: F) -> Server
where
    F: Fn(&Received) -> Vec<u8> + Send + Sync + 'static,
{
    let reply = Arc::new(reply);
    serve(move |mut conn, _, seen| {
        let reply = reply.clone();
        async move {
            while let Some(request) = conn.request().await {
                let bytes = reply(&request);
                seen.lock().unwrap().push(request);
                if !conn.send(&bytes).await {
                    return;
                }
            }
        }
    })
    .await
}

/// A proxy that records every request head. A CONNECT is answered with
/// `connect_reply` when given; otherwise it is accepted and tunnelled to
/// 127.0.0.1 at the CONNECT target's port. Any other request gets `reply`.
pub async fn proxy(connect_reply: Option<&'static [u8]>, reply: &'static [u8]) -> Server {
    serve(move |mut conn, _, seen| async move {
        while let Some(request) = conn.request().await {
            let is_connect = request.head.method == "CONNECT";
            let target = request.head.target.clone();
            seen.lock().unwrap().push(request);
            if !is_connect {
                if !conn.send(reply).await {
                    return;
                }
                continue;
            }
            if let Some(refusal) = connect_reply {
                conn.send(refusal).await;
                return;
            }
            let port: u16 = target.rsplit(':').next().unwrap().parse().unwrap();
            let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", port)).await else {
                conn.send(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            };
            if !conn
                .send(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
            {
                return;
            }
            // Bytes the client sent after the CONNECT head, if any, go first.
            let early = std::mem::take(&mut conn.buf);
            if !early.is_empty() && upstream.write_all(&early).await.is_err() {
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut conn.io, &mut upstream).await;
            return;
        }
    })
    .await
}

/// A TLS origin on 127.0.0.1 negotiating h2 (ALPN `h2` only) and answering
/// every request `200` with `body` and an `x-version` header naming the
/// protocol the server saw.
pub async fn serve_h2_tls(body: &'static str) -> Server {
    let acceptor = tls_acceptor(&[b"h2"]);
    serve(move |conn, _, _| {
        let acceptor = acceptor.clone();
        async move {
            let Ok(tls) = acceptor.accept(conn.io).await else {
                return;
            };
            let service = hyper::service::service_fn(move |request: http::Request<_>| {
                let version = format!("{:?}", request.version());
                async move {
                    let _: &hyper::body::Incoming = request.body();
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .header("x-version", version)
                            .body(http_body_util::Full::new(bytes::Bytes::from_static(
                                body.as_bytes(),
                            )))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                .await;
        }
    })
    .await
}
