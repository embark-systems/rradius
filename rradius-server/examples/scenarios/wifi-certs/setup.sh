#!/bin/sh
# Generate the files this scenario needs before it starts and that cannot live in the repo, in the directory of this script (or the directory given as the first argument):
#   certs/ca.pem, ca.key       demo CA
#   certs/server.pem, .key     RADIUS server certificate (radius.corp.example.com)
#   certs/laptop-001.pem, .key a valid laptop certificate
#   certs/laptop-002.pem, .key a revoked laptop certificate (a lost laptop)
#   certs/ca.crl               revocation list (CRL) containing laptop-002
# Existing files are not overwritten. For testing only: in production use your company CA, and keep private keys on the server.
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

# Each certificate gets a key and a CSR first, then is issued with x509; this works with both LibreSSL and OpenSSL. The CA signs itself.
openssl req -newkey rsa:2048 -nodes -subj "/CN=Example Corp Wi-Fi CA" -keyout ca.key -out ca.csr 2>/dev/null
openssl x509 -req -in ca.csr -signkey ca.key -days 3650 -extfile ext.cnf -extensions ca -out ca.pem 2>/dev/null
rm ca.csr
for name in server laptop-001 laptop-002; do
  openssl req -newkey rsa:2048 -nodes -subj "/CN=$name" -keyout "$name.key" -out "$name.csr" 2>/dev/null
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
    -extfile ext.cnf -extensions "$name" -out "$name.pem" 2>/dev/null
  rm "$name.csr"
done

# Revoke laptop-002 and generate the CRL. openssl ca needs a small database that records revocations, kept in crl-db/.
# To revoke another one later: openssl ca -config crl-db/ca.cnf -revoke <certificate>, then run the -gencrl line again,
# put the new ca.crl on every node and send SIGHUP.
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
