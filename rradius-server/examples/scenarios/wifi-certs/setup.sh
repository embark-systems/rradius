#!/bin/sh
# 產生這個情境啟動前需要、又不能放進 repo 的檔案，放在這個腳本所在的目錄（或第一個參數指定的目錄）：
#   certs/ca.pem、ca.key       示範用的 CA
#   certs/server.pem、.key     RADIUS 伺服器憑證（radius.corp.example.com）
#   certs/laptop-001.pem、.key 一張正常的筆電憑證
#   certs/laptop-002.pem、.key 一張已經撤銷的筆電憑證（遺失的筆電）
#   certs/ca.crl               撤銷清單，裡面有 laptop-002
# 已經有的檔案不會覆蓋。只供測試：正式環境請用公司的 CA，私鑰不要離開伺服器。
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here}
mkdir -p "$out/certs"
umask 077
cd "$out/certs"
if [ -e ca.pem ]; then
  echo "$out/certs/ca.pem is already there, left as it is"
  exit 0
fi

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

[laptop-001]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = clientAuth
subjectAltName = DNS:laptop-001.corp.example.com

[laptop-002]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = clientAuth
subjectAltName = DNS:laptop-002.corp.example.com
CNF

# 每張憑證都先產生金鑰和 CSR，再用 x509 簽發，LibreSSL 和 OpenSSL 都能用。CA 自己簽自己。
openssl req -newkey rsa:2048 -nodes -subj "/CN=Example Corp Wi-Fi CA" -keyout ca.key -out ca.csr 2>/dev/null
openssl x509 -req -in ca.csr -signkey ca.key -days 3650 -extfile ext.cnf -extensions ca -out ca.pem 2>/dev/null
rm ca.csr
for name in server laptop-001 laptop-002; do
  openssl req -newkey rsa:2048 -nodes -subj "/CN=$name" -keyout "$name.key" -out "$name.csr" 2>/dev/null
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
    -extfile ext.cnf -extensions "$name" -out "$name.pem" 2>/dev/null
  rm "$name.csr"
done

# 撤銷 laptop-002 並產生 CRL。openssl ca 要一個記錄撤銷的小資料庫，放在 crl-db/。
# 之後再撤銷一張：openssl ca -config crl-db/ca.cnf -revoke <憑證>，再執行 -gencrl 那一行，
# 把新的 ca.crl 放到每個節點並送 SIGHUP。
mkdir crl-db
: > crl-db/index.txt
echo 01 > crl-db/crlnumber
cat > crl-db/ca.cnf <<CNF
[ca]
default_ca = this
[this]
database = $out/certs/crl-db/index.txt
crlnumber = $out/certs/crl-db/crlnumber
certificate = $out/certs/ca.pem
private_key = $out/certs/ca.key
default_md = sha256
default_crl_days = 30
CNF
openssl ca -config crl-db/ca.cnf -revoke laptop-002.pem 2>/dev/null
openssl ca -config crl-db/ca.cnf -gencrl -out ca.crl 2>/dev/null
rm -f ext.cnf ca.srl
echo "wrote certs in $out/certs: ca.pem, ca.crl, server, laptop-001, laptop-002 (revoked)"
