# Local TLS interoperability fixture

`test-cert.der` is a self-signed RSA certificate for `DNS:localhost`, generated
for these tests on 2026-10-24 and valid for 3650 days. `test-key.der` is its
**public, disposable test key**, encoded as unencrypted PKCS#8. It authenticates
no real service and must never be installed or used outside the fixture server.
The test adds this certificate only to a private test `RootCertStore`; production
uses the bundled WebPKI roots and normal verification.

Generated locally with OpenSSL, with subject `/CN=localhost`, SAN `DNS:localhost`,
critical `basicConstraints=CA:FALSE`, critical
`keyUsage=digitalSignature,keyEncipherment` and `extendedKeyUsage=serverAuth`.
No third-party certificate, credential or content is included.

The integration test uses TLS 1.2 and 1.3, negotiates `h2` ALPN, forces TLS 1.3
HelloRetryRequest with a server restricted to P-256, and exchanges application
data through a 31-byte duplex transport. It proves TLS interoperability and
backpressure behavior, not packet boundaries or bypass of a real filter.
