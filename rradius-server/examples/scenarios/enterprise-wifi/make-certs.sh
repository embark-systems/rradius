#!/bin/sh
# 產生示範用的 CA、RADIUS 伺服器憑證和一張裝置憑證，放在 certs/（或第一個參數指定的目錄）。
# 只供測試。正式環境請用公司的 CA，私鑰不要離開伺服器。
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

# 每張憑證都先產生金鑰和 CSR，再用 x509 簽發，LibreSSL 和 OpenSSL 都能用。CA 自己簽自己。
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
