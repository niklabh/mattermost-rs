// Command licensed is the pinned Go server with exactly one substitution: the public key its
// licence validator trusts.
//
// # Why this exists
//
// `scripts/go-server.sh` builds `cmd/mattermost` with no `-ldflags`, so `model.BuildEnterpriseReady`
// is empty and `PlatformService.LoadLicense` is never called (platform/service.go:371). The stack's
// Go server is therefore Team Edition in the strict sense: it cannot be licensed by any row, file
// or environment variable, and every licence-gated route on it answers the refusal. That left the
// licensed half of some forty routes with no oracle at all — the ledger entries D-300, D-360,
// D-371, D-390 and D-413 all record the same gap.
//
// A licence Go will load has to verify against `license-public-key.txt` or its `-test` sibling
// (channels/utils/license.go:96), and neither private key is in the tree. But `utils.LicenseValidator`
// is a package variable, replaced by Go's own test suite for the same reason, and everything else
// about licence handling — `ValidateAndSetLicenseBytes`, `Features.SetDefaults`, `GetClientLicense`,
// every `MinimumXLicense` gate — runs unchanged on whatever the validator hands back. So this binary
// is `cmd/mattermost` plus a validator that reads its key from `MMRS_LICENSE_PUBLIC_KEY_FILE`, built
// with `BuildEnterpriseReady=true`. `scripts/go-licensed.sh` generates the key pair, signs a licence
// with it, and starts this on a spare port beside the stack's ordinary server.
//
// `mm-api` reads the same variable and trusts the same key, so the two sides of a comparison are
// verifying the same licence the same way; only the key material differs from a release, which is
// exactly how Go itself differs between its production and test service environments.
//
// # What is copied and why
//
// `keyedValidator.ValidateLicense` is `LicenseValidatorImpl.ValidateLicense` (utils/license.go:71)
// with `licenseKeysForEnvironment` replaced by the one key. Everything else — the null-terminator
// strip, the 256-byte split, SHA-512, PKCS#1 v1.5 — is verbatim, so a licence this accepts is one the
// real validator would accept with the matching key. The wrong-environment branch collapses to the
// plain "Invalid signature" error because there is no second key to try.
package main

import (
	"crypto"
	"crypto/rsa"
	"crypto/sha512"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"net/http"
	"os"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/utils"
	"github.com/mattermost/mattermost/server/v8/cmd/mattermost/commands"

	// The same registrations `cmd/mattermost/main.go` makes, so the server is the server.
	_ "github.com/mattermost/mattermost/server/v8/channels/app/oauthproviders/gitlab"
	_ "github.com/mattermost/mattermost/server/v8/channels/app/slashcommands"
	_ "github.com/mattermost/mattermost/server/v8/enterprise"
)

const keyEnv = "MMRS_LICENSE_PUBLIC_KEY_FILE"

type keyedValidator struct {
	publicKey []byte
}

func (v *keyedValidator) LicenseFromBytes(licenseBytes []byte) (*model.License, *model.AppError) {
	licenseStr, err := v.ValidateLicense(licenseBytes)
	if err != nil {
		return nil, utils.NewLicenseValidationAppError("LicenseFromBytes", err)
	}

	var license model.License
	if err := json.Unmarshal([]byte(licenseStr), &license); err != nil {
		return nil, model.NewAppError("LicenseFromBytes", "api.unmarshal_error", nil, "", http.StatusInternalServerError).Wrap(err)
	}

	return &license, nil
}

func (v *keyedValidator) ValidateLicense(signed []byte) (string, error) {
	decoded := make([]byte, base64.StdEncoding.DecodedLen(len(signed)))

	_, err := base64.StdEncoding.Decode(decoded, signed)
	if err != nil {
		return "", fmt.Errorf("encountered error decoding license: %w", err)
	}

	// remove null terminator
	for len(decoded) > 0 && decoded[len(decoded)-1] == byte(0) {
		decoded = decoded[:len(decoded)-1]
	}

	if len(decoded) <= 256 {
		return "", fmt.Errorf("Signed license not long enough")
	}

	plaintext := decoded[:len(decoded)-256]
	signature := decoded[len(decoded)-256:]

	h := sha512.New()
	h.Write(plaintext)
	d := h.Sum(nil)

	if err := verifyLicenseSignature(v.publicKey, d, signature); err != nil {
		if !errors.Is(err, rsa.ErrVerification) {
			return "", err
		}
		return "", fmt.Errorf("Invalid signature: %w", err)
	}

	return string(plaintext), nil
}

// verifyLicenseSignature is utils.verifyLicenseSignature (utils/license.go:148), verbatim.
func verifyLicenseSignature(publicKey, digest, signature []byte) error {
	block, _ := pem.Decode(publicKey)
	if block == nil {
		return fmt.Errorf("failed to decode public key PEM block")
	}

	public, err := x509.ParsePKIXPublicKey(block.Bytes)
	if err != nil {
		return fmt.Errorf("encountered error parsing public key: %w", err)
	}

	rsaPublic, ok := public.(*rsa.PublicKey)
	if !ok {
		return fmt.Errorf("public key is not an RSA key")
	}

	return rsa.VerifyPKCS1v15(rsaPublic, crypto.SHA512, digest, signature)
}

func main() {
	keyFile := os.Getenv(keyEnv)
	if keyFile == "" {
		fmt.Fprintf(os.Stderr, "%s is not set; this binary exists only to trust a stack-local key\n", keyEnv)
		os.Exit(2)
	}
	publicKey, err := os.ReadFile(keyFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "reading %s: %v\n", keyFile, err)
		os.Exit(2)
	}
	utils.LicenseValidator = &keyedValidator{publicKey: publicKey}

	if err := commands.Run(os.Args[1:]); err != nil {
		os.Exit(1)
	}
}
