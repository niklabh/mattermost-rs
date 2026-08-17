package main

// Behavioural oracle for model/mention_map.go, written to fixtures/behaviour_mention_map.json.
//
// Two `map[string]string` newtypes and a pair of codecs that move them through URL query
// parameters. The types are trivial; the codecs are not, and four things about them only a
// measurement settles:
//
//  1. **The four key names are unexported constants**, so no amount of calling the package
//     reveals them directly. They are recovered here the way `ToURLValues` exposes them — by
//     encoding a one-entry map — rather than transcribed from the source on trust.
//
//  2. **`mentionsFromURLValues` distinguishes three shapes of "missing".** Neither key present is
//     success with an *empty, non-nil* map. One key present without the other is an error naming
//     the missing one. Both present with different lengths is a third error. Getting the first of
//     those wrong turns a mention-free request into a 400.
//
//  3. **A repeated mention is only an error when the ids disagree.** The guard is
//     `ok && oldId != id`, so the same pair twice is fine and silently collapses.
//
//  4. **`mentionsToURLValues` ranges a Go map, so its output order is RANDOM.** `Values.Encode`
//     sorts by key, but there are only two keys and the *slice* under each preserves insertion
//     order — which is map-iteration order. A two-entry mention map therefore encodes two ways
//     from one input. That is a Go fact rather than something a fixture may record (see D-032 on
//     nondeterministic fixtures), so what is recorded is `encode_sorted`: the same pairs added in
//     sorted-by-mention order. The Rust port's `BTreeMap` produces exactly that, and for the
//     cases where Go's own output IS deterministic (zero or one entry) the fixture records both
//     and they must agree.

import (
	"encoding/json"
	"net/url"
	"os"
	"path/filepath"
	"sort"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeMentionMapBehaviourFixture(outDir string) error {
	out := map[string]any{
		"keys":        mentionMapKeys(),
		"from_values": mentionMapFromValuesAll(),
		"to_values":   mentionMapToValuesAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_mention_map.json"), append(blob, '\n'), 0o644)
}

// mentionMapKeys recovers the four unexported key constants. `ToURLValues` on a one-entry map
// emits exactly `<mentionKey>=<mention>&<idKey>=<id>` — one entry, so no map-order ambiguity —
// which names both keys for each type.
func mentionMapKeys() map[string]any {
	user := model.UserMentionMap{"m": "i"}.ToURLValues()
	channel := model.ChannelMentionMap{"m": "i"}.ToURLValues()

	// Values.Encode sorts by key, so the two names come back in byte order, not declaration
	// order. Recording the raw encoding as well keeps the derivation checkable.
	return map[string]any{
		"user_encoded":    user.Encode(),
		"user_keys":       sortedKeys(user),
		"channel_encoded": channel.Encode(),
		"channel_keys":    sortedKeys(channel),
	}
}

func sortedKeys(v url.Values) []string {
	keys := make([]string, 0, len(v))
	for k := range v {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

// --- FromURLValues ---------------------------------------------------------------------------

// Shorthands so the corpora below stay a flat list of literals. Go does not elide the type of a
// composite literal used as a struct field, only of one used as a slice or map element.
type (
	qv = map[string][]string
	sm = map[string]string
)

type mentionFromCase struct {
	Name string `json:"name"`
	// The url.Values as a plain map, which is more expressive than a query string: it can carry
	// a key present with an EMPTY slice, which ParseQuery cannot produce and which the `ok`
	// checks treat as present.
	In map[string][]string `json:"in"`
	// Recorded per type — the two differ only in which keys they read, but the corpus drives
	// both so a swapped constant fails a test.
	User    map[string]string `json:"user"`
	UserErr string            `json:"user_err"`
	Channel map[string]string `json:"channel"`
	ChanErr string            `json:"channel_err"`
	// Whether Go returned a nil map alongside its error. Every error path returns nil, and the
	// no-keys path returns an allocated empty map; the two are different values in Go.
	UserNil bool `json:"user_nil"`
	ChanNil bool `json:"channel_nil"`
}

func mentionMapFromValuesAll() []mentionFromCase {
	corpus := []struct {
		name string
		in   map[string][]string
	}{
		{"nil_values", nil},
		{"empty_values", qv{}},
		{"unrelated_keys_only", qv{"page": {"1"}, "per_page": {"60"}}},

		// Neither key present is SUCCESS with an empty map, not an error.
		{"neither_key", qv{"channel_mentions": {"c"}, "channel_mentions_ids": {"ci"}}},

		{"mentions_only", qv{"user_mentions": {"m"}}},
		{"ids_only", qv{"user_mentions_ids": {"i"}}},
		{"both_empty_slices", qv{"user_mentions": {}, "user_mentions_ids": {}}},
		{"mentions_empty_slice_ids_present", qv{"user_mentions": {}, "user_mentions_ids": {"i"}}},

		{"one_pair", qv{"user_mentions": {"town-square"}, "user_mentions_ids": {"cid1"}}},
		{"two_pairs", qv{
			"user_mentions":     {"alice", "bob"},
			"user_mentions_ids": {"id-a", "id-b"},
		}},
		{"length_mismatch_more_mentions", qv{
			"user_mentions":     {"a", "b"},
			"user_mentions_ids": {"id-a"},
		}},
		{"length_mismatch_more_ids", qv{
			"user_mentions":     {"a"},
			"user_mentions_ids": {"id-a", "id-b"},
		}},

		// A repeat is fine when the ids agree and an error when they do not.
		{"duplicate_same_id", qv{
			"user_mentions":     {"a", "a"},
			"user_mentions_ids": {"id-a", "id-a"},
		}},
		{"duplicate_different_id", qv{
			"user_mentions":     {"a", "a"},
			"user_mentions_ids": {"id-a", "id-b"},
		}},
		// Three entries where the clash is in the middle: the error names the FIRST id seen.
		{"duplicate_clash_ordering", qv{
			"user_mentions":     {"a", "a", "a"},
			"user_mentions_ids": {"first", "second", "third"},
		}},

		// Nothing validates the contents: empty strings, ids that are not ids, a `~` prefix.
		{"empty_mention", qv{"user_mentions": {""}, "user_mentions_ids": {"id"}}},
		{"empty_id", qv{"user_mentions": {"a"}, "user_mentions_ids": {""}}},
		{"both_empty_strings", qv{"user_mentions": {""}, "user_mentions_ids": {""}}},
		{"mention_keeps_tilde", qv{"user_mentions": {"~town"}, "user_mentions_ids": {"id"}}},
		{"id_is_not_an_id", qv{"user_mentions": {"a"}, "user_mentions_ids": {"nope"}}},
		{"non_ascii", qv{"user_mentions": {"café", "日本"}, "user_mentions_ids": {"i1", "i2"}}},
		{"whitespace", qv{"user_mentions": {" a b "}, "user_mentions_ids": {"\tid\n"}}},

		// Both families at once: each codec reads only its own pair of keys.
		{"both_families", qv{
			"user_mentions":        {"u"},
			"user_mentions_ids":    {"ui"},
			"channel_mentions":     {"c"},
			"channel_mentions_ids": {"ci"},
		}},
		{"channel_only_pair", qv{"channel_mentions": {"c"}, "channel_mentions_ids": {"ci"}}},
		{"channel_mentions_only", qv{"channel_mentions": {"c"}}},
	}

	res := make([]mentionFromCase, 0, len(corpus))
	for _, c := range corpus {
		out := mentionFromCase{Name: c.name, In: c.in}

		userMap, userErr := model.UserMentionMapFromURLValues(url.Values(c.in))
		out.User = userMap
		out.UserNil = userMap == nil
		if userErr != nil {
			out.UserErr = userErr.Error()
		}

		chanMap, chanErr := model.ChannelMentionMapFromURLValues(url.Values(c.in))
		out.Channel = chanMap
		out.ChanNil = chanMap == nil
		if chanErr != nil {
			out.ChanErr = chanErr.Error()
		}

		res = append(res, out)
	}
	return res
}

// --- ToURLValues -----------------------------------------------------------------------------

type mentionToCase struct {
	Name string            `json:"name"`
	In   map[string]string `json:"in"`
	// The pairs in sorted-by-mention order, which is what the Rust BTreeMap produces.
	Mentions []string `json:"mentions"`
	Ids      []string `json:"ids"`
	// url.Values built by adding those pairs in that order, then Encode()d. Deterministic.
	UserEncodeSorted    string `json:"user_encode_sorted"`
	ChannelEncodeSorted string `json:"channel_encode_sorted"`
	// Go's OWN Encode(), recorded only where map iteration cannot reorder anything (fewer than
	// two entries). Empty string elsewhere, with Deterministic saying which.
	UserEncodeActual    string `json:"user_encode_actual"`
	ChannelEncodeActual string `json:"channel_encode_actual"`
	Deterministic       bool   `json:"deterministic"`
	// The round trip: ToURLValues then FromURLValues must reproduce the input regardless of the
	// order the map was ranged in. This is the property that makes the randomness harmless.
	RoundTrips bool `json:"round_trips"`
}

func mentionMapToValuesAll() []mentionToCase {
	corpus := []struct {
		name string
		in   map[string]string
	}{
		{"nil_map", nil},
		{"empty_map", sm{}},
		{"one_pair", sm{"alice": "id-a"}},
		{"two_pairs", sm{"alice": "id-a", "bob": "id-b"}},
		{"three_pairs", sm{"c": "3", "a": "1", "b": "2"}},
		{"empty_mention", sm{"": "id"}},
		{"empty_id", sm{"a": ""}},
		{"both_empty", sm{"": ""}},
		// Escaping: Encode runs both halves through QueryEscape, so a space becomes `+`.
		{"needs_escaping", sm{"a b": "c&d=e"}},
		{"non_ascii", sm{"café": "日本"}},
		{"tilde_prefix", sm{"~town-square": "cid"}},
		{"plus_and_percent", sm{"a+b": "%41"}},
	}

	res := make([]mentionToCase, 0, len(corpus))
	for _, c := range corpus {
		mentions := make([]string, 0, len(c.in))
		for k := range c.in {
			mentions = append(mentions, k)
		}
		sort.Strings(mentions)
		ids := make([]string, 0, len(mentions))
		for _, m := range mentions {
			ids = append(ids, c.in[m])
		}

		out := mentionToCase{
			Name:                c.name,
			In:                  c.in,
			Mentions:            mentions,
			Ids:                 ids,
			UserEncodeSorted:    encodeSortedPairs(mentions, ids, "user_mentions", "user_mentions_ids"),
			ChannelEncodeSorted: encodeSortedPairs(mentions, ids, "channel_mentions", "channel_mentions_ids"),
			Deterministic:       len(c.in) < 2,
		}
		if out.Deterministic {
			out.UserEncodeActual = model.UserMentionMap(c.in).ToURLValues().Encode()
			out.ChannelEncodeActual = model.ChannelMentionMap(c.in).ToURLValues().Encode()
		}

		// The round trip, run through Go's own randomised ordering.
		back, err := model.UserMentionMapFromURLValues(model.UserMentionMap(c.in).ToURLValues())
		out.RoundTrips = err == nil && sameStringMap(back, c.in)
		res = append(res, out)
	}
	return res
}

// encodeSortedPairs reproduces mentionsToURLValues with a fixed iteration order, so the result is
// deterministic and can be committed. See this file's header note 4.
func encodeSortedPairs(mentions, ids []string, mentionKey, idKey string) string {
	values := url.Values{}
	for i, mention := range mentions {
		values.Add(mentionKey, mention)
		values.Add(idKey, ids[i])
	}
	return values.Encode()
}

func sameStringMap(a model.UserMentionMap, b map[string]string) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range b {
		if a[k] != v {
			return false
		}
	}
	return true
}
