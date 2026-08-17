package main

// Behavioural oracle for the portable half of model/link_metadata.go, written to
// fixtures/behaviour_link_metadata.json.
//
// The other half needs `github.com/dyatlov/go-opengraph`, a third-party package — see [D-105].
// What is covered here is everything that does not touch it, and three of those pieces are traps.
//
// # fnv.New32 is FNV-1, not FNV-1a
//
//	hash := fnv.New32()
//
// Go's `fnv.New32()` returns **FNV-1** (multiply, then XOR); `fnv.New32a()` returns FNV-1a (XOR,
// then multiply). Most libraries in other languages default to 1a, and most people reach for 1a
// without noticing. This value is the LinkMetadata table's primary key, so picking the wrong
// variant silently repartitions the table and every cached link lookup misses. The corpus records
// the hash for a spread of inputs so the variant is pinned by value rather than by name.
//
// Note also the timestamp is written **little-endian** into the hash before the URL bytes, and
// the result is a uint32 widened to int64 — so it is always non-negative, which a naive
// `int64(int32(...))` port would get wrong for hashes above 2^31.
//
// # Not covered here: %.300s
//
// `truncateText` uses `fmt.Sprintf("%.300s[...]", original)`, where Go's precision for `%s` is
// measured in RUNES rather than bytes — a trap worth pinning. It is **unexported**, and its only
// caller is `TruncateOpenGraph`, which is deferred with the rest of the OpenGraph surface, so
// there is no way to drive it from outside the package and nothing yet that needs it. It belongs
// with that half when it lands. See [D-105].
//
// # The hour rounding is UTC and floors
//
// `FloorToNearestHour` converts to UTC before truncating, so the result does not depend on the
// host's zone — worth pinning, because the day-bounds corpus in behaviour_utils.json does ([D-008])
// and the difference is invisible until a test runs in another zone.
//
// Determinism: fixed values only. See [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeLinkMetadataBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":  lmConstants(),
		"keys":       lmKeys(),
		"wire":       lmWireAll(),
		"hash":       lmHashAll(),
		"floor_hour": lmFloorHourAll(),
		"is_svg_url": lmIsSVGURLAll(),
		"is_valid":   lmIsValidAll(),
		"pre_save":   lmPreSaveAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_link_metadata.json"), append(blob, '\n'), 0o644)
}

func lmConstants() map[string]any {
	return map[string]any{
		"LinkMetadataTypeImage":     string(model.LinkMetadataTypeImage),
		"LinkMetadataTypeNone":      string(model.LinkMetadataTypeNone),
		"LinkMetadataTypeOpengraph": string(model.LinkMetadataTypeOpengraph),
		"LinkMetadataMaxImages":     model.LinkMetadataMaxImages,
		"LinkMetadataMaxURLLength":  model.LinkMetadataMaxURLLength,
	}
}

func lmKeys() map[string]any {
	return map[string]any{
		// No json tags at all, so these are the Go field names.
		"link_metadata": expectedKeys(reflect.TypeOf(model.LinkMetadata{})),
	}
}

func lmWireAll() []map[string]any {
	out := make([]map[string]any, 0)
	add := func(name string, v any) {
		blob, err := json.Marshal(v)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": name, "json": string(blob)})
	}

	add("zero", &model.LinkMetadata{})
	add("type_none", &model.LinkMetadata{
		Hash:      1234567890,
		URL:       "https://example.com/page",
		Timestamp: 1700000000000,
		Type:      model.LinkMetadataTypeNone,
	})
	// Data is `any`, so whatever it holds marshals inline. A PostImage is the image case.
	add("type_image", &model.LinkMetadata{
		Hash:      99,
		URL:       "https://example.com/i.png",
		Timestamp: 1700000000000,
		Type:      model.LinkMetadataTypeImage,
		Data:      &model.PostImage{Width: 100, Height: 200, Format: "png", FrameCount: 1},
	})
	return out
}

// --- GenerateLinkMetadataHash ------------------------------------------------------------------

func lmHashAll() []map[string]any {
	corpus := []struct {
		url       string
		timestamp int64
	}{
		{"", 0},
		{"https://example.com", 0},
		{"", 1700000000000},
		{"https://example.com", 1700000000000},
		{"https://example.com", 1700000003600},
		{"https://example.com/", 1700000000000},
		{"https://example.com/a", 1700000000000},
		// A one-bit difference in the timestamp must move the hash.
		{"https://example.com", 1700000000001},
		// Non-ASCII in the URL: the bytes are hashed, not the runes.
		{"https://example.com/café", 1700000000000},
		// Long URL, to reach past a single hash block.
		{"https://example.com/" + lmRepeat("a", 500), 1700000000000},
		// Negative and large timestamps, since the value is written little-endian as int64.
		{"https://example.com", -1},
		{"https://example.com", 9223372036854775807},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		hash := model.GenerateLinkMetadataHash(c.url, c.timestamp)
		out = append(out, map[string]any{
			"url":       c.url,
			"timestamp": c.timestamp,
			"hash":      hash,
			// Recorded so a port that sign-extends a uint32 into int64 is caught: every value
			// here must be non-negative.
			"non_negative": hash >= 0,
		})
	}
	return out
}

func lmRepeat(s string, n int) string {
	out := make([]byte, 0, len(s)*n)
	for range n {
		out = append(out, s...)
	}
	return string(out)
}

// --- FloorToNearestHour ------------------------------------------------------------------------

func lmFloorHourAll() []map[string]any {
	corpus := []int64{
		0,
		1,
		3599999,
		3600000,
		3600001,
		1700000000000,
		// Exactly on an hour.
		1700002800000,
		// Negative, i.e. before the epoch — Go's time.Date still floors, downward.
		-1,
		-3600000,
		-3600001,
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, ms := range corpus {
		floored := model.FloorToNearestHour(ms)
		out = append(out, map[string]any{
			"input":      ms,
			"floored":    floored,
			"is_rounded": floored == ms,
		})
	}
	return out
}

// --- IsSVGImageURL -----------------------------------------------------------------------------

func lmIsSVGURLAll() []map[string]any {
	corpus := []string{
		"",
		"https://example.com/a.svg",
		"https://example.com/a.svgz",
		"https://example.com/a.SVG",
		"https://example.com/a.SvGz",
		"https://example.com/a.png",
		"https://example.com/a.svg.png",
		"https://example.com/a.png.svg",
		// The QUERY is not part of the path, so this is not an SVG.
		"https://example.com/a.png?x=.svg",
		// ...and a fragment is not either.
		"https://example.com/a.png#.svg",
		// A path that is only the extension.
		"https://example.com/.svg",
		"/relative/a.svg",
		"a.svg",
		// Percent-encoded: url.Parse decodes Path, so this IS an svg.
		"https://example.com/a%2Esvg",
		// Not parseable.
		"://nope",
		"https://example.com/a b.svg",
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, u := range corpus {
		out = append(out, map[string]any{"url": u, "is_svg": model.IsSVGImageURL(u)})
	}
	return out
}

// --- IsValid / PreSave -------------------------------------------------------------------------

func lmIsValidAll() []map[string]any {
	valid := func() model.LinkMetadata {
		return model.LinkMetadata{
			URL:       "https://example.com/page",
			Timestamp: model.FloorToNearestHour(1700000000000),
			Type:      model.LinkMetadataTypeNone,
		}
	}

	corpus := []struct {
		name string
		in   model.LinkMetadata
	}{
		{"valid_none", valid()},
		{"empty_url", func() model.LinkMetadata { m := valid(); m.URL = ""; return m }()},
		{"url_at_cap", func() model.LinkMetadata {
			m := valid()
			m.URL = "https://e.com/" + lmRepeat("a", model.LinkMetadataMaxURLLength-len("https://e.com/"))
			return m
		}()},
		{"url_over_cap", func() model.LinkMetadata {
			m := valid()
			m.URL = "https://e.com/" + lmRepeat("a", model.LinkMetadataMaxURLLength)
			return m
		}()},
		{"zero_timestamp", func() model.LinkMetadata { m := valid(); m.Timestamp = 0; return m }()},
		// Not on an hour boundary.
		{"unrounded_timestamp", func() model.LinkMetadata {
			m := valid()
			m.Timestamp = 1700000000001
			return m
		}()},
		{"type_none_with_data", func() model.LinkMetadata {
			m := valid()
			m.Data = &model.PostImage{}
			return m
		}()},
		{"type_image_without_data", func() model.LinkMetadata {
			m := valid()
			m.Type = model.LinkMetadataTypeImage
			return m
		}()},
		{"type_image_with_post_image", func() model.LinkMetadata {
			m := valid()
			m.Type = model.LinkMetadataTypeImage
			m.Data = &model.PostImage{Width: 1, Height: 1}
			return m
		}()},
		// The wrong concrete type in an `any` field.
		{"type_image_with_wrong_data", func() model.LinkMetadata {
			m := valid()
			m.Type = model.LinkMetadataTypeImage
			m.Data = "a string"
			return m
		}()},
		// A non-pointer PostImage is NOT a *PostImage, so the type assertion fails.
		{"type_image_with_value_not_pointer", func() model.LinkMetadata {
			m := valid()
			m.Type = model.LinkMetadataTypeImage
			m.Data = model.PostImage{Width: 1, Height: 1}
			return m
		}()},
		{"unknown_type", func() model.LinkMetadata {
			m := valid()
			m.Type = "something-else"
			return m
		}()},
		{"empty_type", func() model.LinkMetadata { m := valid(); m.Type = ""; return m }()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		err := in.IsValid()
		entry := map[string]any{"name": c.name}
		if err == nil {
			entry["ok"] = true
		} else {
			entry["ok"] = false
			entry["id"] = err.Id
			entry["where"] = err.Where
			entry["status"] = err.StatusCode
		}
		out = append(out, entry)
	}
	return out
}

func lmPreSaveAll() []map[string]any {
	m := model.LinkMetadata{
		URL:       "https://example.com/page",
		Timestamp: 1700000000000,
	}
	m.PreSave()

	return []map[string]any{{
		"name": "pre_save",
		"hash": m.Hash,
		// PreSave sets only the hash.
		"url":       m.URL,
		"timestamp": m.Timestamp,
		"matches_generate": m.Hash == model.GenerateLinkMetadataHash(
			"https://example.com/page", 1700000000000,
		),
	}}
}
