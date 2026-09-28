package main

// Behavioural oracle for the generated initials avatar (D-204) — written to
// fixtures/behaviour_avatar.json and replayed by `crates/gofont` and `mm_app`.
//
// Three layers, so a mismatch can be located:
//
//   - `avatars`: the real `UserService.GetDefaultProfileImage` — FNV-1a colour, the 64pt face over
//     fonts/nunito-bold.ttf, `GlyphBounds` centring, `font.Drawer`, `png.BestCompression` — as
//     PNG bytes (SHA-256, length, and the whole file base64 for a few). Every one of the 26 colour
//     slots and a spread of first characters, including a first byte that is half of a UTF-8
//     sequence (Go converts the **byte** to a rune) and a bot (the embedded image).
//   - `glyphs`: freetype's `truetype.NewFace` directly, at several sizes and sub-pixel dots —
//     `GlyphBounds`, `GlyphAdvance`, and `Glyph`'s rectangle, advance and mask bytes. This is what
//     pins the scan converter and the glyph loader independently of the avatar's one size.
//   - `font`: the parsed font's own numbers.
//
// The UserService is built with stand-in stores that are never called: `GetDefaultProfileImage`
// reads only the configuration. `FindDir("fonts")` searches from the working directory, so the
// call runs with the working directory moved to the server tree, and moved back.
//
// Determinism: fixed inputs; nothing reads the clock.

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"image"
	"os"
	"path/filepath"

	"github.com/golang/freetype/truetype"
	"golang.org/x/image/font"
	"golang.org/x/image/math/fixed"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app/users"
	"github.com/mattermost/mattermost/server/v8/channels/store"
)

type avatarUserStore struct{ store.UserStore }
type avatarSessionStore struct{ store.SessionStore }
type avatarOAuthStore struct{ store.OAuthStore }

func avatarSHA(b []byte) string {
	s := sha256.Sum256(b)
	return hex.EncodeToString(s[:])
}

func writeAvatarBehaviourFixture(outDir string) error {
	serverDir, err := filepath.Abs("../mattermost/server")
	if err != nil {
		return err
	}
	fontBytes, err := os.ReadFile(filepath.Join(serverDir, "fonts", "nunito-bold.ttf"))
	if err != nil {
		return fmt.Errorf("read font: %w", err)
	}

	avatars, err := avatarCases(serverDir)
	if err != nil {
		return err
	}
	glyphs, fontInfo, err := glyphCases(fontBytes)
	if err != nil {
		return err
	}

	out := map[string]any{
		"font_file":   "fonts/nunito-bold.ttf",
		"font_sha256": avatarSHA(fontBytes),
		"font":        fontInfo,
		"avatars":     avatars,
		"glyphs":      glyphs,
	}
	blob, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal avatar behaviour: %w", err)
	}
	blob = append(blob, '\n')
	return os.WriteFile(filepath.Join(outDir, "behaviour_avatar.json"), blob, 0o644)
}

func avatarCases(serverDir string) ([]map[string]any, error) {
	cfg := &model.Config{}
	cfg.SetDefaults()
	svc, err := users.New(users.ServiceConfig{
		UserStore:    avatarUserStore{},
		SessionStore: avatarSessionStore{},
		OAuthStore:   avatarOAuthStore{},
		ConfigFn:     func() *model.Config { return cfg },
		LicenseFn:    func() *model.License { return nil },
	})
	if err != nil {
		return nil, err
	}

	wd, err := os.Getwd()
	if err != nil {
		return nil, err
	}
	if err := os.Chdir(serverDir); err != nil {
		return nil, err
	}
	defer func() { _ = os.Chdir(wd) }()

	type probe struct {
		username string
		userID   string
		isBot    bool
		font     string
	}
	var probes []probe
	// Every colour slot: ids whose FNV-1a lands on 0..25 in turn, found by search so the slot is
	// the generator's arithmetic and not a claim.
	for slot := 0; slot < 26; slot++ {
		for n := 0; ; n++ {
			id := fmt.Sprintf("avatarslot%02dn%012d", slot, n)
			if fnv32a(id)%26 == uint32(slot) {
				probes = append(probes, probe{username: fmt.Sprintf("user%02d", slot), userID: id})
				break
			}
		}
	}
	// A spread of first characters on one id.
	for _, name := range []string{
		"alice", "bob", "zed", "Mixed", "0day", "9lives", "_under", ".dot", "-dash",
		"q", "w", "m", "i", "j", "g", "y", "@at", "%pct", "élan", "ßharp", "émile",
		"日本", "",
	} {
		probes = append(probes, probe{username: name, userID: "avatarfixedidxxxxxxxxxxxxx"})
	}
	// The old default font name is read as the new one.
	probes = append(probes, probe{username: "legacy", userID: "avatarlegacyfontxxxxxxxxxx", font: "luximbi.ttf"})
	// A bot: the embedded image, whatever its name.
	probes = append(probes, probe{username: "botty", userID: "avatarbotxxxxxxxxxxxxxxxxx", isBot: true})

	var out []map[string]any
	for i, p := range probes {
		if p.font != "" {
			cfg.FileSettings.InitialFont = model.NewPointer(p.font)
		} else {
			cfg.FileSettings.InitialFont = model.NewPointer("nunito-bold.ttf")
		}
		row := map[string]any{
			"username": p.username,
			"user_id":  p.userID,
			"is_bot":   p.isBot,
			"font":     *cfg.FileSettings.InitialFont,
			"seed":     fnv32a(p.userID),
		}
		img, err := func() (b []byte, err error) {
			defer func() {
				if r := recover(); r != nil {
					err = fmt.Errorf("panic: %v", r)
				}
			}()
			return svc.GetDefaultProfileImage(&model.User{Id: p.userID, Username: p.username, IsBot: p.isBot})
		}()
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["png_sha256"] = avatarSHA(img)
			row["png_len"] = len(img)
			if i < 3 || p.isBot || p.username == "élan" {
				row["png_base64"] = base64.StdEncoding.EncodeToString(img)
			}
		}
		out = append(out, row)
	}
	return out, nil
}

func fnv32a(s string) uint32 {
	h := uint32(2166136261)
	for i := 0; i < len(s); i++ {
		h ^= uint32(s[i])
		h *= 16777619
	}
	return h
}

func glyphCases(fontBytes []byte) ([]map[string]any, map[string]any, error) {
	f, err := truetype.Parse(fontBytes)
	if err != nil {
		return nil, nil, err
	}
	runes := []rune{'A', 'B', 'M', 'W', 'Q', 'g', 'j', 'y', '0', '8', '@', '%', '&', ' ', 'Ã', 'é', 'ß', 'Ø', '€', '中', '_', '.', '-'}
	sizes := []float64{8, 12, 17.3, 33, 64, 100, 150}
	dots := []fixed.Point26_6{
		{X: fixed.I(10), Y: fixed.I(50)},
		{X: fixed.I(10) + 17, Y: fixed.I(50) + 33},
		{X: fixed.I(-3) + 40, Y: fixed.I(7) + 5},
	}

	var out []map[string]any
	for _, size := range sizes {
		face := truetype.NewFace(f, &truetype.Options{Size: size})
		for _, r := range runes {
			row := map[string]any{"size": size, "rune": string(r), "index": int(f.Index(r))}
			if b, adv, ok := face.GlyphBounds(r); ok {
				row["bounds"] = []int32{int32(b.Min.X), int32(b.Min.Y), int32(b.Max.X), int32(b.Max.Y)}
				row["bounds_advance"] = int32(adv)
			} else {
				row["bounds"] = nil
			}
			adv, aok := face.GlyphAdvance(r)
			row["advance"] = int32(adv)
			row["advance_ok"] = aok
			var glyphs []map[string]any
			for _, dot := range dots {
				dr, mask, maskp, gadv, ok := face.Glyph(dot, r)
				g := map[string]any{"dot": []int32{int32(dot.X), int32(dot.Y)}, "ok": ok}
				if ok {
					g["dr"] = []int{dr.Min.X, dr.Min.Y, dr.Max.X, dr.Max.Y}
					g["advance"] = int32(gadv)
					g["mask_sha256"] = avatarSHA(maskBytes(mask.(*image.Alpha), dr, maskp))
				}
				glyphs = append(glyphs, g)
			}
			row["glyph"] = glyphs
			out = append(out, row)
		}
		face.Close()
	}

	size64, dpi := 64.0, 72.0
	scale := fixed.Int26_6(0.5 + (size64 * dpi * 64 / 72))
	b := f.Bounds(scale)
	face64 := truetype.NewFace(f, &truetype.Options{Size: 64})
	m := face64.Metrics()
	info := map[string]any{
		"units_per_em": f.FUnitsPerEm(),
		"bounds_64":    []int32{int32(b.Min.X), int32(b.Min.Y), int32(b.Max.X), int32(b.Max.Y)},
		"metrics_64":   []int32{int32(m.Height), int32(m.Ascent), int32(m.Descent)},
		"hinting_none": face64.Kern('A', 'V') == face64.Kern('A', 'V'),
		"kern_av_64":   int32(face64.Kern('A', 'V')),
	}
	_ = font.HintingNone
	return out, info, nil
}

// The mask's bytes under the glyph rectangle, row by row: what DrawMask reads.
func maskBytes(m *image.Alpha, dr image.Rectangle, mp image.Point) []byte {
	var out []byte
	for y := 0; y < dr.Dy(); y++ {
		i := m.PixOffset(mp.X, mp.Y+y)
		out = append(out, m.Pix[i:i+dr.Dx()]...)
	}
	return out
}
