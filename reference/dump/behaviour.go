package main

// Behavioural oracle: runs a corpus of inputs through the real Go implementations and
// records what they returned, to fixtures/behaviour_utils.json.
//
// The serialization fixtures in main.go pin the wire *shape*. This pins the wire *logic* —
// the validators, sanitizers and error rendering that a Rust port can get subtly wrong while
// still compiling and round-tripping. Reading a regex and reasoning about what it accepts is
// exactly the step that produces confident, wrong translations; this replaces the reasoning
// with Go's own answer.
//
// The five identifier patterns are unexported in the model package, so they are recompiled
// here verbatim from utils.go:709-715. Copy any change to them character for character —
// the Rust side asserts against these results, so a silent edit here weakens the test rather
// than failing it.
//
// Non-ASCII inputs are written as \u escapes, never as literals: several are invisible, and
// a literal BOM is rejected by the Go compiler outright.

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"regexp"

	"github.com/mattermost/mattermost/server/public/model"
)

var (
	validAlphaNum                           = regexp.MustCompile(`^[a-z0-9]+([a-z\-0-9]+|(__)?)[a-z0-9]+$`)
	validAlphaNumHyphenUnderscore           = regexp.MustCompile(`^[a-z0-9]+([a-z\-\_0-9]+|(__)?)[a-z0-9]+$`)
	validSimpleAlphaNum                     = regexp.MustCompile(`^[a-z0-9]+([a-z\-\_0-9]+|(__)?)[a-z0-9]*$`)
	validSimpleAlphaNumHyphenUnderscore     = regexp.MustCompile(`^[a-zA-Z0-9\-_]+$`)
	validSimpleAlphaNumHyphenUnderscorePlus = regexp.MustCompile(`^[a-zA-Z0-9+_-]+$`)
)

// identifierCorpus deliberately includes the boundary shapes the alternation in those
// patterns turns on: single characters, leading/trailing separators, single vs double
// underscore, and mixed case.
var identifierCorpus = []string{
	"", "a", "ab", "t1", "1", "12", "test", "Test", "TEST", "tEst",
	"test-name", "test--name", "test_name", "test__name", "test___name",
	"-test", "test-", "_test", "test_", "a-", "-a", "a_", "_a",
	"test name", "test+name", "a.b", "a1-b2_c3", "a--b", "a__b__c",
	"__", "--", "a__", "__a", "ab_", "_ab",
}

var idCorpus = []string{
	"",
	"a",
	repeat("a", 25),
	repeat("a", 26),
	repeat("a", 27),
	repeat("a", 25) + "-",
	repeat("a", 25) + " ",
	repeat("1", 26),
	"ybndrfg8ejkmcpqxot1uwisza34",
	"umrmxsgrzx9hkuqks9ccjx89aw",
	repeat("é", 13), // 26 bytes, 13 runes, category L
	repeat("ͅ", 13), // 26 bytes, Other_Alphabetic but category Mn, NOT L
	repeat("́", 13), // 26 bytes, combining acute, category Mn
}

type limitCase struct {
	In   string `json:"in"`
	Max  int    `json:"max"`
	Out  string `json:"out"`
	Cut  bool   `json:"cut"`
	Kind string `json:"kind"`
}

type appErrorCase struct {
	Where    string `json:"where"`
	ID       string `json:"id"`
	Message  string `json:"message"`
	Detailed string `json:"detailed"`
	Wrapped  string `json:"wrapped"`
	Display  string `json:"display"`
	ToJSON   string `json:"to_json"`
}

func writeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"valid_alpha_num":                               matchAll(validAlphaNum),
		"valid_alpha_num_hyphen_underscore":             matchAll(validAlphaNumHyphenUnderscore),
		"valid_simple_alpha_num":                        matchAll(validSimpleAlphaNum),
		"valid_simple_alpha_num_hyphen_underscore":      matchAll(validSimpleAlphaNumHyphenUnderscore),
		"valid_simple_alpha_num_hyphen_underscore_plus": matchAll(validSimpleAlphaNumHyphenUnderscorePlus),
		"is_valid_id":                                   isValidIDAll(),
		"pad_date_string_zeros":                         padAll(),
		"clear_mention_tags":                            clearMentionAll(),
		"sanitize_unicode":                              sanitizeAll(),
		"get_preferred_timezone":                        timezoneAll(),
		"limits":                                        limitAll(),
		"app_errors":                                    appErrorAll(),
		"day_bounds":                                    dayBoundsAll(),
		"is_valid_username":                             usernameAll(false),
		"is_valid_username_allow_remote":                usernameAll(true),
		"is_in_role":                                    isInRoleAll(),
		"get_roles":                                     getRolesAll(),
		"user_display_names":                            displayNameAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_utils.json"), append(blob, '\n'), 0o644)
}

func matchAll(re *regexp.Regexp) map[string]bool {
	res := make(map[string]bool, len(identifierCorpus))
	for _, in := range identifierCorpus {
		res[in] = re.MatchString(in)
	}
	return res
}

func isValidIDAll() map[string]bool {
	res := make(map[string]bool, len(idCorpus))
	for _, in := range idCorpus {
		res[in] = model.IsValidId(in)
	}
	return res
}

func padAll() map[string]string {
	res := map[string]string{}
	for _, in := range []string{"", "2019-1-2", "2019-11-12", "2019-1-12", "1-1-1", "2019", "a-b"} {
		res[in] = model.PadDateStringZeros(in)
	}
	return res
}

func clearMentionAll() map[string]string {
	res := map[string]string{}
	for _, in := range []string{
		"", "no tags", "<mention>hi</mention>", "<mention><mention>x",
		"</mention>only-close", "<mentions>not a tag</mentions>",
	} {
		res[in] = model.ClearMentionTags(in)
	}
	return res
}

func sanitizeAll() map[string]string {
	res := map[string]string{}
	for _, in := range []string{
		"hello", "h\u00e9llo \u2603",
		// Every code point on the blocklist (utils.go:865).
		"a\u0340b", "a\u0341b", "a\u17a3b", "a\u17d3b", "a\u2028b", "a\u2029b",
		"a\u202ab", "a\u202bb", "a\u202cb", "a\u202db", "a\u202eb", "a\u206ab",
		"a\u206bb", "a\u206cb", "a\u206db", "a\u206eb", "a\u206fb", "a\ufff9b",
		"a\ufffab", "a\ufffbb", "a\ufeffb", "a\ufffcb",
		// Both ends of each blocklisted range, plus the code point just outside it.
		"a\U0001d173b", "a\U0001d17ab", "a\U0001d172b", "a\U0001d17bb",
		"a\U000e0000b", "a\U000e007fb", "a\U000dffffb", "a\U000e0080b",
		// Near misses that must survive untouched.
		"a\u033fb", "a\u0342b", "a\u17a2b", "a\u17d4b", "a\u2027b", "a\u202fb",
		"a\u2069b", "a\u2070b", "a\ufff8b", "a\ufffdb", "a\ufffeb",
	} {
		res[in] = model.SanitizeUnicode(in)
	}
	return res
}

func timezoneAll() map[string]string {
	cases := map[string]model.StringMap{
		"automatic": {"useAutomaticTimezone": "true", "automaticTimezone": "America/New_York", "manualTimezone": "Europe/Berlin"},
		"manual":    {"useAutomaticTimezone": "false", "automaticTimezone": "America/New_York", "manualTimezone": "Europe/Berlin"},
		"empty":     {},
		"missing":   {"useAutomaticTimezone": "true"},
		"truthy":    {"useAutomaticTimezone": "TRUE", "manualTimezone": "Europe/Berlin"},
	}
	res := map[string]string{}
	for name, tz := range cases {
		res[name] = model.GetPreferredTimezone(tz)
	}
	return res
}

func limitAll() []limitCase {
	var res []limitCase
	for _, c := range []struct {
		in  string
		max int
	}{
		{"hello", 10}, {"hello", 3}, {"hello", 0}, {"", 3},
		{"héllo", 2}, {"héllo", 10}, {"aé", 2}, {"☃☃☃", 2},
	} {
		out, cut := model.LimitRunes(c.in, c.max)
		res = append(res, limitCase{In: c.in, Max: c.max, Out: out, Cut: cut, Kind: "runes"})
	}
	// LimitBytes can split a multi-byte rune. Record what Go actually produced so the Rust
	// side's documented divergence is measured against a real value, not an assumption.
	for _, c := range []struct {
		in  string
		max int
	}{
		{"hello", 10}, {"hello", 3}, {"", 3}, {"aé", 2}, {"aé", 3},
	} {
		out, cut := model.LimitBytes(c.in, c.max)
		res = append(res, limitCase{In: c.in, Max: c.max, Out: out, Cut: cut, Kind: "bytes"})
	}
	return res
}

func appErrorAll() []appErrorCase {
	build := func(where, id, message, detailed, wrapped string) appErrorCase {
		er := model.NewAppError(where, id, nil, detailed, 400)
		if message != "" {
			er.Message = message
		}
		if wrapped != "" {
			er = er.Wrap(errors.New(wrapped))
		}
		return appErrorCase{
			Where: where, ID: id, Message: message, Detailed: detailed, Wrapped: wrapped,
			Display: er.Error(), ToJSON: er.ToJSON(),
		}
	}
	return []appErrorCase{
		build("Api.Handler", "an.id", "", "the detail", ""),
		build("", "an.id", "", "", ""),
		build("", "an.id", "", "detail", ""),
		build("W", "an.id", model.NoTranslation, "detail", ""),
		build("W", "an.id", model.NoTranslation, "", ""),
		build("W", "an.id", "", "detail", "inner boom"),
		build("W", "an.id", "", "", "inner boom"),
		build("", "an.id", "", repeat("x", 2000), ""),
	}
}

func dayBoundsAll() []map[string]any {
	var res []map[string]any
	for _, millis := range []int64{1700000000000, 0, 1234567890123} {
		for _, offset := range []int{0, 3600, -3600, 19800} {
			// GetTimeForMillis wraps time.UnixMilli, which returns a Time in **Local**.
			// GetStartOfDayMillis then reads Y/M/D from that local calendar, so the answer
			// depends on the server's timezone. Record the offset that was in effect, or
			// these numbers are unreproducible on another machine.
			t := model.GetTimeForMillis(millis)
			_, localOffset := t.Zone()
			res = append(res, map[string]any{
				"millis":       millis,
				"offset":       offset,
				"local_offset": localOffset,
				"start":        model.GetStartOfDayMillis(t, offset),
				"end":          model.GetEndOfDayMillis(t, offset),
			})
		}
	}
	return res
}

func repeat(s string, n int) string {
	out := make([]byte, 0, len(s)*n)
	for range n {
		out = append(out, s...)
	}
	return string(out)
}

// --- user.go -----------------------------------------------------------------

var usernameCorpus = []string{
	"", "a", "ada", "Ada", "ADA", "ada.lovelace", "ada-lovelace", "ada_lovelace",
	"ada:remote", "ada@example", "ada lovelace", "ada!", "1", "1ada", ".ada", "ada.",
	"-ada", "ada-", "_ada", "ada_", "..", "--", "__",
	"all", "channel", "matterbot", "system", "All", "channels",
	repeat("a", 63), repeat("a", 64), repeat("a", 65),
	"\u00e9ada", "ada\u00e9",
}

func usernameAll(allowRemote bool) map[string]bool {
	res := make(map[string]bool, len(usernameCorpus))
	for _, in := range usernameCorpus {
		if allowRemote {
			res[in] = model.IsValidUsernameAllowRemote(in)
		} else {
			res[in] = model.IsValidUsername(in)
		}
	}
	return res
}

// roleCorpus exercises the separator difference between IsInRole (Split on " ") and
// GetRoles (strings.Fields).
var roleCorpus = []string{
	"", "system_user", "system_user system_admin", "system_user  system_admin",
	"system_user\tsystem_admin", "system_user\nsystem_admin", " system_user ",
	"system_guest", "system_user system_guest", "system_admin",
}

func isInRoleAll() map[string]bool {
	res := map[string]bool{}
	for _, roles := range roleCorpus {
		for _, want := range []string{"system_user", "system_admin", "system_guest"} {
			res[roles+"|"+want] = model.IsInRole(roles, want)
		}
	}
	return res
}

func getRolesAll() map[string][]string {
	res := map[string][]string{}
	for _, roles := range roleCorpus {
		u := &model.User{Roles: roles}
		got := u.GetRoles()
		if got == nil {
			got = []string{}
		}
		res[roles] = got
	}
	return res
}

func displayNameAll() map[string]string {
	type person struct{ username, first, last, nickname string }
	people := map[string]person{
		"full":          {"ada", "Ada", "Lovelace", "countess"},
		"no_nickname":   {"ada", "Ada", "Lovelace", ""},
		"first_only":    {"ada", "Ada", "", ""},
		"last_only":     {"ada", "", "Lovelace", ""},
		"username_only": {"ada", "", "", ""},
		"nickname_only": {"ada", "", "", "countess"},
	}
	res := map[string]string{}
	for name, p := range people {
		u := &model.User{Username: p.username, FirstName: p.first, LastName: p.last, Nickname: p.nickname}
		for _, format := range []string{model.ShowUsername, model.ShowFullName, model.ShowNicknameFullName} {
			res[name+"|"+format] = u.GetDisplayName(format)
			res[name+"|"+format+"|@"] = u.GetDisplayNameWithPrefix(format, "@")
		}
		res[name+"|fullname"] = u.GetFullName()
	}
	return res
}
