package main

// Behavioural oracle for multi-factor authentication: `platform/shared/mfa` (GenerateSecret,
// Activate, ValidateToken), `github.com/dgryski/dgoogauth` (the TOTP it runs) and
// `github.com/mattermost/rsc/qr` (the QR code and its PNG). Written to fixtures/behaviour_mfa.json
// and asserted by `goqr`'s and `mm_app::mfa`'s `go_parity` modules.
//
// Derived from the **AGPL** half of the tree for the `mfa` rows, so those feed `mm-app` only; the
// `qr` rows are the BSD-licensed `rsc/qr` alone.
//
// # Every value here is Go's own output
//
//   - `qr`: `qr.Encode(text, qr.H).PNG()` for texts that cross every encoding (numeric,
//     alphanumeric, bytes) and a range of versions, plus the one that is too long.
//   - `generate_secret`: `mfa.GenerateSecret` itself, over a store that records what it was asked
//     to write. The secret is `crypto/rand`'s, so it is recorded next to the PNG Go rendered for
//     it: the Rust side renders its own `otpauth://` URL for the same secret, site URL and e-mail
//     and must produce the same bytes — which pins the issuer derivation and the URL layout.
//   - `compute_code`: `dgoogauth.ComputeCode` for fixed secrets and counters.
//   - `validate` and `activate`: `mfa.ValidateToken` and `mfa.Activate` over the real clock. The
//     tokens are computed from the current 30-second step `t0` and every timestamp is recorded
//     **relative to `t0`**, so the fixture is the same on every run; a row whose step changed
//     while it ran is run again.

import (
	"encoding/base64"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/dgryski/dgoogauth"
	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/platform/shared/mfa"
	"github.com/mattermost/rsc/qr"
)

// mfaRecordingStore is `mfa.Store` over fixed used timestamps, recording every write.
type mfaRecordingStore struct {
	used   []int
	calls  []map[string]any
	t0     int
	getErr bool
}

func (s *mfaRecordingStore) rel(ts []int) []int {
	out := make([]int, len(ts))
	for i, t := range ts {
		out[i] = t - s.t0
	}
	return out
}

func (s *mfaRecordingStore) UpdateMfaActive(userId string, active bool) error {
	s.calls = append(s.calls, map[string]any{"call": "UpdateMfaActive", "user_id": userId, "active": active})
	return nil
}

func (s *mfaRecordingStore) UpdateMfaSecret(userId, secret string) error {
	s.calls = append(s.calls, map[string]any{"call": "UpdateMfaSecret", "user_id": userId, "secret": secret})
	return nil
}

func (s *mfaRecordingStore) StoreMfaUsedTimestamps(userId string, ts []int) error {
	s.calls = append(s.calls, map[string]any{"call": "StoreMfaUsedTimestamps", "user_id": userId, "ts": s.rel(ts)})
	return nil
}

func (s *mfaRecordingStore) GetMfaUsedTimestamps(userId string) ([]int, error) {
	if s.getErr {
		return nil, errors.New("store failure")
	}
	out := make([]int, len(s.used))
	copy(out, s.used)
	return out, nil
}

func qrTexts() []string {
	texts := []string{
		"", "0", "1", "12", "123", "0123456789", "01234567890123456789012345678901234567890",
		"A", "HELLO WORLD", "HTTP://EXAMPLE.COM/$%*+-./:", "AB", "ABC",
		"a", "hello", "héllo wörld", "\x00",
		"otpauth://totp/localhost%3A8065:sliceuser@example.com?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=localhost%3A8065",
		"otpauth://totp/Mattermost:a@b.c?secret=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&issuer=Mattermost",
	}
	// Byte-mode lengths across the versions level H holds, and past the last.
	for _, n := range []int{7, 8, 13, 14, 23, 24, 34, 35, 43, 44, 57, 58, 63, 64, 83, 84, 105, 106, 121, 122, 151, 152, 250, 400, 700, 1000, 1273} {
		texts = append(texts, strings.Repeat("x", n))
	}
	// Numeric and alphanumeric at size-class boundaries.
	texts = append(texts, strings.Repeat("7", 200), strings.Repeat("7", 1000), strings.Repeat("Z", 300), strings.Repeat("Z", 700))
	texts = append(texts, strings.Repeat("x", 1274), strings.Repeat("7", 3058))
	return texts
}

func writeMfaBehaviourFixture(outDir string) error {
	qrRows := []map[string]any{}
	for _, text := range qrTexts() {
		row := map[string]any{"text_b64": base64.StdEncoding.EncodeToString([]byte(text))}
		code, err := qr.Encode(text, qr.H)
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["size"] = code.Size
			row["png_b64"] = base64.StdEncoding.EncodeToString(code.PNG())
		}
		qrRows = append(qrRows, row)
	}

	genRows := []map[string]any{}
	for _, c := range []struct{ siteURL, email string }{
		{"", "a@example.com"},
		{"http://localhost:8065", "sliceuser@example.com"},
		{"https://www.example.com", "user+tag@example.com"},
		{"  https://chat.example.com/sub  ", "x@y.z"},
		{"http://www.www.example.com", "o'neil@example.com"},
		{"ftp://example.com", "a b@example.com"},
		{"https://example.com/path?q=1&r=a b", "é@example.com"},
		{"www.example.com", "u@example.com"},
	} {
		store := &mfaRecordingStore{}
		secret, img, err := mfa.New(store).GenerateSecret(c.siteURL, c.email, "userid0000000000000000000a")
		if err != nil {
			return fmt.Errorf("generate: %w", err)
		}
		genRows = append(genRows, map[string]any{
			"site_url": c.siteURL,
			"email":    c.email,
			"secret":   secret,
			"png_b64":  base64.StdEncoding.EncodeToString(img),
			"calls":    store.calls,
		})
	}

	codeRows := []map[string]any{}
	for _, secret := range []string{
		"JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP", "JBSWY3DPEHPK3PXP", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
		"jbswy3dpehpk3pxp", "JBSWY3DPEHPK3PX", "JBSWY3DPEHPK3PX=", "GEZDGNBV", "", "MFRGG===", "01234567",
	} {
		for _, value := range []int64{0, 1, 2, 59, 1111111109, 1234567890 / 30, 2000000000 / 30, -1, 1 << 40} {
			codeRows = append(codeRows, map[string]any{
				"secret": secret, "value": value, "code": dgoogauth.ComputeCode(secret, value),
			})
		}
	}

	const secret = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"
	type tokenCase struct {
		name   string
		offset *int   // the code for t0+offset
		raw    string // or this literal
		pad    string // surrounding whitespace for an offset code
		used   []int  // relative to t0
		secret string
	}
	off := func(k int) *int { return &k }
	cases := []tokenCase{
		{name: "current", offset: off(0)},
		{name: "previous step", offset: off(-1)},
		{name: "next step", offset: off(1)},
		{name: "two back", offset: off(-2)},
		{name: "two ahead", offset: off(2)},
		{name: "current reused", offset: off(0), used: []int{0}},
		{name: "previous reused, current fresh", offset: off(0), used: []int{-1}},
		{name: "old entries dropped", offset: off(0), used: []int{-7, -2, -1}},
		{name: "unsorted list", offset: off(1), used: []int{0, -3, -1}},
		{name: "future entry kept", offset: off(-1), used: []int{5}},
		{name: "padded", offset: off(0), pad: " \t"},
		{name: "wrong code", raw: "000000"},
		{name: "empty", raw: ""},
		{name: "spaces only", raw: "   "},
		{name: "five digits", raw: "12345"},
		{name: "seven digits", raw: "1234567"},
		{name: "eight digits scratch", raw: "12345678"},
		{name: "eight digits leading zero", raw: "01234567"},
		{name: "letters", raw: "12a456"},
		{name: "plus sign", raw: "+12345"},
		{name: "leading zero six", raw: "012345"},
		{name: "unicode digits", raw: "１２３４５６"},
		{name: "bad secret", offset: off(0), secret: "not base32!"},
		{name: "empty secret", offset: off(0), secret: "-"},
	}
	validateRows := []map[string]any{}
	activateRows := []map[string]any{}
	for _, c := range cases {
		for attempt := 0; ; attempt++ {
			if attempt > 5 {
				return errors.New("the clock kept crossing a 30-second step")
			}
			t0 := int(time.Now().Unix() / 30)
			sec := secret
			if c.secret == "-" {
				sec = ""
			} else if c.secret != "" {
				sec = c.secret
			}
			token := c.raw
			if c.offset != nil {
				token = c.pad + fmt.Sprintf("%06d", dgoogauth.ComputeCode(secret, int64(t0+*c.offset))) + c.pad
			}
			used := []int{}
			for _, u := range c.used {
				used = append(used, t0+u)
			}
			vs := &mfaRecordingStore{used: used, t0: t0}
			ok, verr := mfa.New(vs).ValidateToken(&model.User{Id: "userid0000000000000000000a", MfaSecret: sec}, token)
			as := &mfaRecordingStore{used: used, t0: t0}
			aerr := mfa.New(as).Activate(sec, "userid0000000000000000000a", token)
			if int(time.Now().Unix()/30) != t0 {
				continue
			}
			used0 := c.used
			if used0 == nil {
				used0 = []int{}
			}
			base := map[string]any{"name": c.name, "secret": sec, "token_offset": c.offset, "token_pad": c.pad, "token_raw": c.raw, "used": used0}
			v := map[string]any{"ok": ok, "error": verr != nil, "calls": vs.calls}
			for k, x := range base {
				v[k] = x
			}
			a := map[string]any{"error": aerr != nil, "invalid_token": errors.Is(aerr, mfa.InvalidToken), "calls": as.calls}
			for k, x := range base {
				a[k] = x
			}
			validateRows = append(validateRows, v)
			activateRows = append(activateRows, a)
			break
		}
	}

	return writeJSONFixture(outDir, "behaviour_mfa.json", map[string]any{
		"qr":              qrRows,
		"generate_secret": genRows,
		"compute_code":    codeRows,
		"validate":        validateRows,
		"activate":        activateRows,
	})
}
