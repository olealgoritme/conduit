#!/usr/bin/env bash
# Create the self-signed test code-signing certificate for nvddi.sys.
#   sign/mkcert.sh            -> sign/out/nvddi-test.{key,crt,cer}
# nvddi-test.cer (DER) is what the VM imports into Root and TrustedPublisher.
# The private key stays on this host (sign/out is not tracked by git).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/out"
mkdir -p "$out"
chmod 700 "$out"
if [ -f "$out/nvddi-test.key" ]; then
    echo "mkcert: $out/nvddi-test.key exists, keeping it"
else
    cat > "$out/cert.cnf" <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = nvddi test signing
O = nvddi
[ext]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = codeSigning
subjectKeyIdentifier = hash
CNF
    openssl req -x509 -newkey rsa:3072 -sha256 -days 3650 -nodes \
        -keyout "$out/nvddi-test.key" -out "$out/nvddi-test.crt" -config "$out/cert.cnf"
    chmod 600 "$out/nvddi-test.key"
fi
openssl x509 -in "$out/nvddi-test.crt" -outform DER -out "$out/nvddi-test.cer"
openssl x509 -in "$out/nvddi-test.crt" -noout -subject -enddate -fingerprint -sha1
echo "mkcert: import $out/nvddi-test.cer into the VM's Root and TrustedPublisher stores"
