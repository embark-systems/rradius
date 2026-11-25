#!/bin/sh
# Create a demo CA, a RADIUS server certificate and one device certificate, in certs/ (or the directory given as the first argument).
# For testing only. In production use the company's CA, and the private key must not leave the server.
set -eu
out=${1:-"$(dirname "$0")/certs"}
mkdir -p "$out"
cd "$out"
umask 077

cat > ext.cnf <<'CNF'
[ca]
basicConstraints = critical, CA:true
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash

[server]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:radius.corp.example.com

[device]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = clientAuth
subjectAltName = DNS:laptop-001.corp.example.com
CNF

# Each certificate first gets a key and a CSR, then is issued with x509, which works in both LibreSSL and OpenSSL. The CA signs itself.
openssl req -newkey rsa:2048 -nodes -subj "/CN=Example Corp Wi-Fi CA" -keyout ca.key -out ca.csr 2>/dev/null
openssl x509 -req -in ca.csr -signkey ca.key -days 3650 -extfile ext.cnf -extensions ca -out ca.pem 2>/dev/null
rm ca.csr

for name in server device; do
  openssl req -newkey rsa:2048 -nodes -subj "/CN=$name" -keyout "$name.key" -out "$name.csr" 2>/dev/null
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
    -extfile ext.cnf -extensions "$name" -out "$name.pem" 2>/dev/null
  rm "$name.csr"
done
rm -f ext.cnf ca.srl
echo "certs in $out: ca.pem, server.pem/.key, device.pem/.key"
