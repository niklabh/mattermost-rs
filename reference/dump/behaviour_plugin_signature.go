package main

// Behavioural oracle for plugin signature verification (channels/app/plugin_signature.go) and
// go-is-svg's `Is` (the icon check of `getIcon`, channels/app/plugin.go:1230), written to
// fixtures/behaviour_plugin_signature.json.
//
// `verifySignature`, `verifyBinarySignature` and `decodeIfArmored` are unexported, so their
// bodies are transcribed below line for line (plugin_signature.go:121-154); everything they call
// is golang.org/x/crypto/openpgp at the pinned version, which is the thing the oracle is for. The
// corpus signs two plugin bundles with a fixed test key and walks the forms Go accepts: armored
// and binary keys and signatures, a text-mode signature, a stranger's signature, a stranger's
// followed by ours in one file, a signature over the other bundle, garbage and empty inputs.
//
// The two keys are TEST KEYS generated once with this module's x/crypto (RSA 2048, 2023-11-14)
// and pasted here; nothing trusts them outside this repository. The bundles and signatures are
// also written out, so `parity::marketplace` can serve them from its mock Marketplace and plant
// the public key as a configuration file. Determinism: fixed key, fixed signing time, fixed tar
// headers and a gzip header with no name or time; RSA PKCS#1 v1.5 signatures are deterministic.

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto"
	"encoding/base64"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	svg "github.com/h2non/go-is-svg"
	"github.com/pkg/errors"
	"golang.org/x/crypto/openpgp"       //nolint:staticcheck
	"golang.org/x/crypto/openpgp/armor" //nolint:staticcheck
	"golang.org/x/crypto/openpgp/packet"
)

const pluginSigTestKey = `-----BEGIN PGP PRIVATE KEY BLOCK-----

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
=T8aT
-----END PGP PRIVATE KEY BLOCK-----`

const pluginSigStrangerKey = `-----BEGIN PGP PRIVATE KEY BLOCK-----

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
=vfjS
-----END PGP PRIVATE KEY BLOCK-----`

// --- transcribed from channels/app/plugin_signature.go:121-154 ---

func sigVerifySignature(publicKey, message, signature io.Reader) error {
	pk, err := sigDecodeIfArmored(publicKey)
	if err != nil {
		return errors.Wrap(err, "can't decode public key")
	}
	s, err := sigDecodeIfArmored(signature)
	if err != nil {
		return errors.Wrap(err, "can't decode signature")
	}
	return sigVerifyBinarySignature(pk, message, s)
}

func sigVerifyBinarySignature(publicKey, signedFile, signature io.Reader) error {
	keyring, err := openpgp.ReadKeyRing(publicKey)
	if err != nil {
		return errors.Wrap(err, "can't read public key")
	}
	if _, err = openpgp.CheckDetachedSignature(keyring, signedFile, signature); err != nil {
		return errors.Wrap(err, "error while checking the signature")
	}
	return nil
}

func sigDecodeIfArmored(reader io.Reader) (io.Reader, error) {
	readBytes, err := io.ReadAll(reader)
	if err != nil {
		return nil, errors.Wrap(err, "can't read the file")
	}
	block, err := armor.Decode(bytes.NewReader(readBytes))
	if err != nil {
		return bytes.NewReader(readBytes), nil
	}
	return block.Body, nil
}

// --- end of transcription ---

func sigEntity(armored string) *openpgp.Entity {
	list, err := openpgp.ReadArmoredKeyRing(bytes.NewReader([]byte(armored)))
	if err != nil || len(list) != 1 {
		panic("behaviour_plugin_signature: bad test key")
	}
	return list[0]
}

func sigPublic(e *openpgp.Entity, armored bool) []byte {
	var buf bytes.Buffer
	if !armored {
		if err := e.Serialize(&buf); err != nil {
			panic(err)
		}
		return buf.Bytes()
	}
	w, err := armor.Encode(&buf, openpgp.PublicKeyType, nil)
	if err != nil {
		panic(err)
	}
	if err := e.Serialize(w); err != nil {
		panic(err)
	}
	w.Close()
	return buf.Bytes()
}

// sigBundle is a webapp-only plugin bundle: `plugin/plugin.json` and `plugin/dist/main.js`.
func sigBundle(manifest, js string) []byte {
	var raw bytes.Buffer
	gz := gzip.NewWriter(&raw)
	tw := tar.NewWriter(gz)
	mtime := time.Unix(1700000000, 0).UTC()
	add := func(name string, typ byte, mode int64, body string) {
		hdr := &tar.Header{Name: name, Typeflag: typ, Mode: mode, Size: int64(len(body)), ModTime: mtime, Format: tar.FormatUSTAR}
		if err := tw.WriteHeader(hdr); err != nil {
			panic(err)
		}
		if _, err := tw.Write([]byte(body)); err != nil {
			panic(err)
		}
	}
	add("plugin/", tar.TypeDir, 0o755, "")
	add("plugin/plugin.json", tar.TypeReg, 0o644, manifest)
	add("plugin/dist/", tar.TypeDir, 0o755, "")
	add("plugin/dist/main.js", tar.TypeReg, 0o644, js)
	if err := tw.Close(); err != nil {
		panic(err)
	}
	if err := gz.Close(); err != nil {
		panic(err)
	}
	return raw.Bytes()
}

func sigSign(e *openpgp.Entity, message []byte, armored, text bool) []byte {
	cfg := &packet.Config{DefaultHash: crypto.SHA256, Time: func() time.Time { return time.Unix(1700000100, 0) }}
	var buf bytes.Buffer
	var err error
	switch {
	case armored:
		err = openpgp.ArmoredDetachSign(&buf, e, bytes.NewReader(message), cfg)
	case text:
		err = openpgp.DetachSignText(&buf, e, bytes.NewReader(message), cfg)
	default:
		err = openpgp.DetachSign(&buf, e, bytes.NewReader(message), cfg)
	}
	if err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// sigSignWithSubkey signs with the entity's first subkey, naming it as the issuer.
func sigSignWithSubkey(e *openpgp.Entity, message []byte) []byte {
	sub := e.Subkeys[0]
	cfg := &packet.Config{DefaultHash: crypto.SHA256, Time: func() time.Time { return time.Unix(1700000100, 0) }}
	sig := &packet.Signature{
		SigType:      packet.SigTypeBinary,
		PubKeyAlgo:   sub.PrivateKey.PubKeyAlgo,
		Hash:         crypto.SHA256,
		CreationTime: cfg.Now(),
		IssuerKeyId:  &sub.PublicKey.KeyId,
	}
	h := sig.Hash.New()
	h.Write(message)
	if err := sig.Sign(h, sub.PrivateKey, cfg); err != nil {
		panic(err)
	}
	var buf bytes.Buffer
	if err := sig.Serialize(&buf); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

type sigCase struct {
	Name      string `json:"name"`
	Key       string `json:"key"`
	Message   string `json:"message"`
	Signature string `json:"signature"`
	Verified  bool   `json:"verified"`
	Err       string `json:"err"`
}

type svgIsCase struct {
	Input string `json:"input"`
	Is    bool   `json:"is"`
}

func writePluginSignatureBehaviourFixture(outDir string) error {
	ours := sigEntity(pluginSigTestKey)
	stranger := sigEntity(pluginSigStrangerKey)

	b64 := base64.StdEncoding.EncodeToString
	bundles := map[string][]byte{
		"alpha": sigBundle(`{"id":"mmrs.market.alpha","name":"MMRS Market Alpha","description":"A marketplace probe","version":"1.2.0","webapp":{"bundle_path":"dist/main.js"}}`, "// alpha\n"),
		"beta":  sigBundle(`{"id":"mmrs.market.beta","name":"mmrs market beta","version":"0.4.0","webapp":{"bundle_path":"dist/main.js"}}`, "// beta\n"),
	}
	keys := map[string][]byte{
		"armored":  sigPublic(ours, true),
		"binary":   sigPublic(ours, false),
		"stranger": sigPublic(stranger, true),
		"both":     append(sigPublic(stranger, false), sigPublic(ours, false)...),
		"garbage":  []byte("not a key"),
		"empty":    {},
	}
	signatures := map[string][]byte{
		"alpha_binary":   sigSign(ours, bundles["alpha"], false, false),
		"alpha_armored":  sigSign(ours, bundles["alpha"], true, false),
		"alpha_text":     sigSign(ours, bundles["alpha"], false, true),
		"alpha_stranger": sigSign(stranger, bundles["alpha"], false, false),
		"beta_binary":    sigSign(ours, bundles["beta"], false, false),
		"beta_armored":   sigSign(ours, bundles["beta"], true, false),
		"garbage":        []byte("not a signature"),
		"empty":          {},
	}
	signatures["alpha_stranger_then_ours"] = append(append([]byte{}, signatures["alpha_stranger"]...), signatures["alpha_binary"]...)
	signatures["alpha_ours_then_stranger"] = append(append([]byte{}, signatures["alpha_binary"]...), signatures["alpha_stranger"]...)
	signatures["alpha_armored_with_preamble"] = append([]byte("some text before the armor\n\n"), signatures["alpha_armored"]...)
	// Made with the encryption subkey: the key is in the ring, but its binding signature does not
	// allow signing, so `KeysByIdUsage` offers nothing for its id.
	signatures["alpha_encryption_subkey"] = sigSignWithSubkey(ours, bundles["alpha"])
	// The armor's CRC-24 line replaced by another well-formed one.
	armored := string(signatures["alpha_armored"])
	crcAt := strings.LastIndex(armored, "\n=")
	signatures["alpha_armored_bad_crc"] = []byte(armored[:crcAt] + "\n=AAAA" + armored[crcAt+6:])
	whole := signatures["alpha_binary"]
	signatures["alpha_truncated"] = append([]byte{}, whole[:len(whole)-20]...)
	flipped := append([]byte{}, whole...)
	flipped[len(flipped)-5] ^= 0x01
	signatures["alpha_flipped"] = flipped

	type combo struct{ key, message, signature string }
	combos := []combo{
		{"armored", "alpha", "alpha_binary"},
		{"armored", "alpha", "alpha_armored"},
		{"binary", "alpha", "alpha_binary"},
		{"binary", "alpha", "alpha_armored"},
		{"armored", "alpha", "alpha_text"},
		{"armored", "alpha", "alpha_stranger"},
		{"stranger", "alpha", "alpha_stranger"},
		{"stranger", "alpha", "alpha_binary"},
		{"both", "alpha", "alpha_binary"},
		{"both", "alpha", "alpha_stranger"},
		{"armored", "alpha", "alpha_stranger_then_ours"},
		{"armored", "alpha", "alpha_ours_then_stranger"},
		{"armored", "alpha", "alpha_armored_with_preamble"},
		{"armored", "alpha", "alpha_armored_bad_crc"},
		{"armored", "alpha", "alpha_encryption_subkey"},
		{"armored", "alpha", "beta_binary"},
		{"armored", "beta", "beta_binary"},
		{"armored", "beta", "beta_armored"},
		{"armored", "beta", "alpha_binary"},
		{"armored", "alpha", "alpha_truncated"},
		{"armored", "alpha", "alpha_flipped"},
		{"armored", "alpha", "garbage"},
		{"armored", "alpha", "empty"},
		{"garbage", "alpha", "alpha_binary"},
		{"empty", "alpha", "alpha_binary"},
	}
	cases := make([]sigCase, 0, len(combos))
	for _, c := range combos {
		err := sigVerifySignature(bytes.NewReader(keys[c.key]), bytes.NewReader(bundles[c.message]), bytes.NewReader(signatures[c.signature]))
		sc := sigCase{Name: c.key + "/" + c.message + "/" + c.signature, Key: c.key, Message: c.message, Signature: c.signature, Verified: err == nil}
		if err != nil {
			sc.Err = err.Error()
		}
		cases = append(cases, sc)
	}

	svgInputs := []string{
		"<svg></svg>",
		"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\"><rect/></svg>",
		"  \n<?xml version=\"1.0\"?>\n<!DOCTYPE svg PUBLIC \"x\">\n<svg>a</svg>\n",
		"<SVG></SVG>",
		"<!-- c --><svg></svg>",
		"<!-- <svg></svg> -->",
		"<!-- a --><svg><!-- * --></svg><!-- b -->",
		"<svg>*</svg>",
		"<svg></svg><p>",
		"<html><svg></svg></html>",
		"<svg>",
		"\x01<svg></svg>                        ",
		"<svg></svg>\x01                        ",
		"\x01<svg></svg>",
		"\x08<svg></svg>                        ",
		"\x09<svg></svg>                        ",
		// A control byte inside the element, within the first 24 bytes: only the binary check
		// refuses it, and 8 is the highest byte that check counts.
		"<svg>\x08                   </svg>",
		"<svg>\x09                   </svg>",
		"<svg>é</svg>",
		"<ſvg></ſvg>",
		" <svg></svg>",
		"\v<svg></svg>",
		"<svg>\xff</svg>",
		"<svg \xff></svg>",
		"",
		"\x89PNG\r\n\x1a\n0000000000000000000000",
	}
	svgCases := make([]svgIsCase, 0, len(svgInputs))
	for _, in := range svgInputs {
		svgCases = append(svgCases, svgIsCase{Input: b64([]byte(in)), Is: svg.Is([]byte(in))})
	}

	enc := func(m map[string][]byte) map[string]string {
		out := make(map[string]string, len(m))
		for k, v := range m {
			out[k] = b64(v)
		}
		return out
	}
	out := map[string]any{
		"bundles":    enc(bundles),
		"keys":       enc(keys),
		"signatures": enc(signatures),
		"verify":     cases,
		"svg_is":     svgCases,
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_plugin_signature.json"), append(data, '\n'), 0o644)
}
