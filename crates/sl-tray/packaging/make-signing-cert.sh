#!/bin/sh
# Create a self-signed code-signing identity for the tray, in the login
# keychain, unless one with this name is already there.
#
#   make-signing-cert.sh ["Statusline Dev"]
#
# Why: the tray keeps the GitHub token in the Keychain, readable by the tray
# alone (anything else is asked for your password). The Keychain knows an app
# by its code signature. An ad-hoc signature changes with every build, so each
# rebuilt tray would be a stranger to the token; one signed with the same
# identity every time is not. The certificate never leaves this Mac and is
# trusted by nothing: it only has to stay the same.
#
# The key is made in a temporary directory, imported, and the files are
# deleted. No program is allowed to use the key without asking: every
# signing, `make install` included, shows a prompt for your login password.
# Were codesign allowed silently, any program you run could sign its own app
# as the tray and be handed the token without a prompt.
set -eu

NAME="${1:-Statusline Dev}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
OPENSSL=/usr/bin/openssl

if security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
    echo "\"$NAME\" is already in $KEYCHAIN"
    exit 0
fi

DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT
cat > "$DIR/cfg" <<EOF
[req]
distinguished_name = dn
prompt = no
x509_extensions = ext
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
"$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$DIR/cfg" \
    -keyout "$DIR/key.pem" -out "$DIR/cert.pem" 2>/dev/null
PASS="$("$OPENSSL" rand -hex 16)"
"$OPENSSL" pkcs12 -export -name "$NAME" -inkey "$DIR/key.pem" -in "$DIR/cert.pem" \
    -out "$DIR/id.p12" -passout "pass:$PASS"
security import "$DIR/id.p12" -k "$KEYCHAIN" -P "$PASS"
echo "created \"$NAME\" in $KEYCHAIN; make install signs with it from now on"
