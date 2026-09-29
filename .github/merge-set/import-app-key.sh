#!/usr/bin/env bash
# Imports a private key of the merge-set App into the KMS key of the webhook
# stack (webhook-stack.yml), and checks that KMS holds that key.
#
# Run it in AWS CloudShell, in us-east-1, as a user with the administrator
# role in the account of the stack. Generate a private key in the App
# settings, upload the .pem file that GitHub gives to CloudShell, and run
#
#   bash import-app-key.sh <file>.pem
#
# Then delete the .pem file, in CloudShell and where you downloaded it. KMS
# never gives out the key, so no copy of it is left.
set -euo pipefail
umask 077

STACK_NAME=peppy-merge-set-webhook
REGION=us-east-1

if [[ $# -ne 1 || ! -f $1 ]]; then
  echo "usage: $0 <private key of the App>.pem" >&2
  exit 2
fi
pem=$1

key_id=$(aws cloudformation describe-stacks --region "$REGION" \
  --stack-name "$STACK_NAME" \
  --query "Stacks[0].Outputs[?OutputKey=='AppKeyId'].OutputValue" \
  --output text)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# GitHub gives the key as PKCS#1 PEM, and KMS takes it as PKCS#8 DER.
openssl pkcs8 -topk8 -nocrypt -in "$pem" -outform DER -out "$work/key.der"

# KMS takes an RSA private key wrapped with RSA_AES_KEY_WRAP_SHA_256: a new
# AES key wraps the key, and the wrapping key of KMS encrypts the AES key. The
# wrapping key and the import token come from one call and go together.
aws kms get-parameters-for-import --region "$REGION" --key-id "$key_id" \
  --wrapping-algorithm RSA_AES_KEY_WRAP_SHA_256 --wrapping-key-spec RSA_4096 \
  --query '[PublicKey, ImportToken]' --output text >"$work/parameters"
cut -f1 "$work/parameters" | base64 -d >"$work/wrapping-key.der"
cut -f2 "$work/parameters" | base64 -d >"$work/import-token.bin"

openssl rand -out "$work/aes-key.bin" 32
openssl enc -id-aes256-wrap-pad \
  -K "$(od -An -tx1 -v "$work/aes-key.bin" | tr -d ' \n')" \
  -iv A65959A6 \
  -in "$work/key.der" -out "$work/key-wrapped.bin"
openssl pkeyutl -encrypt \
  -in "$work/aes-key.bin" -out "$work/aes-key-wrapped.bin" \
  -inkey "$work/wrapping-key.der" -keyform DER -pubin \
  -pkeyopt rsa_padding_mode:oaep \
  -pkeyopt rsa_oaep_md:sha256 \
  -pkeyopt rsa_mgf1_md:sha256
cat "$work/aes-key-wrapped.bin" "$work/key-wrapped.bin" >"$work/material.bin"

aws kms import-key-material --region "$REGION" --key-id "$key_id" \
  --encrypted-key-material "fileb://$work/material.bin" \
  --import-token "fileb://$work/import-token.bin" \
  --expiration-model KEY_MATERIAL_DOES_NOT_EXPIRE

# KMS gives the public key of the key it holds: the public key of the .pem
# file proves that it holds the key of the App.
aws kms get-public-key --region "$REGION" --key-id "$key_id" \
  --query PublicKey --output text | base64 -d >"$work/kms-public.der"
openssl pkey -in "$pem" -pubout -outform DER -out "$work/pem-public.der"
if ! cmp -s "$work/kms-public.der" "$work/pem-public.der"; then
  echo "The KMS key $key_id holds another key than $pem." >&2
  exit 1
fi
echo "The KMS key $key_id holds the key of the App. Delete $pem now."
