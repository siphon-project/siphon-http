Certificates for the TLS tests. They identify nothing and protect nothing: the
private keys are in this directory on purpose.

- `ca.pem`: the CA the test listener trusts for client certificates and the
  test client trusts for the server.
- `server.pem`, `server.key`: the listener, `CN=localhost` with the
  alternative names `localhost` and `127.0.0.1`.
- `client.pem`, `client.key`: a client of that CA, with a DNS, a URI, an IP
  and an email alternative name.
- `untrusted-client.pem`, `untrusted-client.key`: a client of a CA the
  listener does not trust.

All are P-256 and valid until 2126. The tests compare what a handler reads
with what `openssl x509 -noout -subject -nameopt RFC2253 -ext subjectAltName
-fingerprint -sha256 -in client.pem` prints.
