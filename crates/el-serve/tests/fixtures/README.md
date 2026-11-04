# TLS test fixtures

These files are used only by `crates/el-serve` tests. They are not secrets and
must never be used to serve real traffic.

- `tls-ca.pem`: self-signed test CA (`CN=e-lite test CA`).
- `tls-cert.pem`: leaf certificate signed by that CA (`CN=localhost`, SAN
  `DNS:localhost`, `IP:127.0.0.1`, `serverAuth`).
- `tls-key.pem`: private key for the leaf certificate.

Regenerate with:

```bash
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout ca-key.pem -out tls-ca.pem -days 7300 \
  -subj "/CN=e-lite test CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl req -newkey rsa:2048 -nodes \
  -keyout tls-key.pem -out leaf.csr \
  -subj "/CN=localhost"

printf '%s\n' \
  'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  'basicConstraints=critical,CA:FALSE' \
  'keyUsage=critical,digitalSignature,keyEncipherment' \
  'extendedKeyUsage=serverAuth' > leaf.ext

openssl x509 -req -in leaf.csr -CA tls-ca.pem -CAkey ca-key.pem \
  -CAcreateserial -out tls-cert.pem -days 7300 -extfile leaf.ext

rm -f ca-key.pem leaf.csr leaf.ext tls-ca.srl
```
