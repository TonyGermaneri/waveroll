#!/usr/bin/env bash
# Sets the five repository secrets a signed release needs. Run it again when the certificate is
# renewed -- a Developer ID expires in five years, by which time nobody remembers how these were
# made, which is the reason this is a script and not a paragraph.
#
#   tools/setup-signing-secrets.sh <cert.p12> <AuthKey_XXXXXX.p8> <key-id> <issuer-id>
#
# Needs `gh auth login` first. Nothing secret is printed or written anywhere: each value goes from
# its file into `gh secret set` over stdin, and the .p12 password is read without echo. Both files
# are checked before anything is uploaded, because a typo here surfaces as a failed release
# twenty minutes into a tag build.
#
# To make the .p12: Keychain Access -> login -> My Certificates -> right-click
# "Developer ID Application: ..." -> Export -> .p12, and set a password when asked. Exporting from
# there gets exactly one identity, which bulk `security export` does not.
#
# To make the .p8: App Store Connect -> Users and Access -> Integrations -> App Store Connect API
# -> generate a key with the Developer role. It downloads once and cannot be downloaded again.
set -euo pipefail

p12="${1:?usage: setup-signing-secrets.sh <cert.p12> <AuthKey.p8> <key-id> <issuer-id>}"
p8="${2:?missing the App Store Connect .p8}"
key_id="${3:?missing the Key ID}"
issuer="${4:?missing the Issuer ID}"

[ -f "$p12" ] || { echo "no such file: $p12" >&2; exit 1; }
[ -f "$p8" ]  || { echo "no such file: $p8" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "run 'gh auth login' first" >&2; exit 1; }

printf 'Password for %s (not echoed): ' "$(basename "$p12")"
read -rs p12_password
printf '\n'

# Checked here rather than discovered in CI. A wrong password fails the keychain import halfway
# through a tag build, and the error it gives names neither the password nor the certificate.
if ! openssl pkcs12 -in "$p12" -passin pass:"$p12_password" -noout -nokeys 2>/dev/null; then
  echo "that password does not open $p12" >&2; exit 1
fi

identities="$(openssl pkcs12 -in "$p12" -passin pass:"$p12_password" -nokeys -clcerts 2>/dev/null \
  | openssl x509 -noout -subject 2>/dev/null | grep -o 'Developer ID Application[^/,]*' || true)"
[ -n "$identities" ] || { echo "$p12 holds no Developer ID Application certificate" >&2; exit 1; }
echo "  certificate: $identities"

grep -q 'BEGIN PRIVATE KEY' "$p8" || { echo "$p8 does not look like an App Store Connect key" >&2; exit 1; }
echo "  key id:      $key_id"
echo "  issuer:      $issuer"

# base64 because a secret is text and these are not. Not encryption -- GitHub does that -- so the
# only thing this protects against is the transport mangling a byte.
base64 -i "$p12" | gh secret set MACOS_CERTIFICATE_P12
printf '%s' "$p12_password" | gh secret set MACOS_CERTIFICATE_PASSWORD
base64 -i "$p8"  | gh secret set APPLE_API_KEY_P8
printf '%s' "$key_id" | gh secret set APPLE_API_KEY_ID
printf '%s' "$issuer" | gh secret set APPLE_API_ISSUER_ID

echo
echo "Set. Names only -- GitHub cannot show a value back, to anyone, ever:"
gh secret list

cat <<'EOF'

Now delete the .p12: it is the one copy of a private key sitting in a folder.

    rm -P <the .p12>

Then cut a tag and the release will come out signed, notarised and stapled.
EOF
