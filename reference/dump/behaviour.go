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
		"is_valid_email":                                emailAll(),
		"is_valid_locale":                               localeAll(),
		"is_valid_email_fuzz":                           emailFuzzAll(),
		"is_reserved_team_name":                         reservedTeamNameAll(),
		"is_valid_team_name":                            validTeamNameAll(),
		"clean_team_name":                               cleanTeamNameAll(),
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

// --- IsValidEmail (utils.go:655) ---------------------------------------------
//
// Mattermost's rule is net/mail.ParseAddress plus three extra constraints: the input must
// already be lowercase, the parsed Address must equal the input verbatim (so no display
// names, angle brackets, comments or requoting), and there must be at most one "@". That
// combination is much narrower than RFC 5322 and is not obvious from reading either half, so
// the corpus below is deliberately hostile.

var emailCorpus = []string{
	// plainly valid
	"a@b.com", "ada@example.com", "ada.lovelace@example.co.uk", "a@b", "a1@b2.c3",
	"ada+tag@example.com", "ada_lovelace@example.com", "ada-lovelace@example.com",
	// every atext special
	"a!b@x.com", "a#b@x.com", "a$b@x.com", "a%b@x.com", "a&b@x.com", "a'b@x.com",
	"a*b@x.com", "a/b@x.com", "a=b@x.com", "a?b@x.com", "a^b@x.com", "a`b@x.com",
	"a{b@x.com", "a|b@x.com", "a}b@x.com", "a~b@x.com", "a+b@x.com",
	// dots in the local part
	".a@x.com", "a.@x.com", "a..b@x.com", "a.b.c@x.com", ".@x.com", "..@x.com",
	// missing pieces
	"", "a", "@", "@x.com", "a@", "a.com", "a@@x.com", "a@b@c.com", "@@",
	// domain shapes
	"a@b.", "a@.b", "a@b..c", "a@-b.com", "a@b-.com", "a@b_c.com", "a@b c.com",
	"a@[127.0.0.1]", "a@[ipv6:::1]", "a@[]", "a@b.c.d.e.f",
	// case
	"A@b.com", "a@B.com", "Ada@Example.com", "a@x.COM",
	// whitespace
	" a@x.com", "a@x.com ", "a b@x.com", "a@x .com", "a\tb@x.com", "a@x.com\n",
	// display names / angle brackets / comments
	"<a@x.com>", "Bob <a@x.com>", "\"Bob\" <a@x.com>", "a@x.com (comment)", "(comment) a@x.com",
	// quoted local parts
	"\"a\"@x.com", "\"a b\"@x.com", "\"a@b\"@x.com", "\"\"@x.com",
	// unicode
	"ünicode@x.com", "a@ünicode.com", "日本@example.com", "a@x.中国",
	// length boundaries (Mattermost caps at 128 elsewhere, ParseAddress does not)
	repeat("a", 64) + "@x.com", repeat("a", 200) + "@x.com", "a@" + repeat("b", 200) + ".com",
	// trailing/leading dots and misc
	"a@x.com.", ".a.@x.com", "a..@x.com", "a@x..com", "-a@x.com", "a-@x.com",
	"_a@x.com", "a_@x.com", "1@2.3", "a@1.2.3.4",
	// second probe batch: domain literals, unicode boundaries, case folding
	"a@[abc]", "a@[1.2.3]", "a@[::1]", "a@[127.0.0.1", "a@127.0.0.1]", "a@[a b]",
	"a@[a\\\\b]", "a@[a[b]", "a@x.com]", "a@[]x", "x[a@b.com",
	"a b@x.com", "a​b@x.com", "\U0001f600@x.com", "a@\U0001f600.com",
	"ß@x.com", "ẞ@x.com", "İ@x.com", "ı@x.com",
	"é́@x.com", "a@x.中国", "日.本@example.com",
	"a..b@x.com", "a@b.c-d", "a@b-c.d", "a@_b.com", "a@b.c_",
	"a-b.c_d@e-f.g_h", "'@x.com", "~@x.com", "|@x.com", "{}@x.com",
	// IP-literal edges: Go validates the bracketed form as an IP address, not as free dtext
	"a@[0.0.0.0]", "a@[255.255.255.255]", "a@[256.1.1.1]", "a@[01.2.3.4]",
	"a@[1.2.3.4.5]", "a@[1.2.3.4 ]", "a@[::ffff:1.2.3.4]", "a@[fe80::1%eth0]",
	"a@[::]", "a@[:::1]", "a@[1::2::3]",
	// separators that look like addresses
	"a,b@x.com", "a;b@x.com", "a:b@x.com", "a<b@x.com", "a>b@x.com", "a[b@x.com",
	"a]b@x.com", "a\\b@x.com", "a\"b@x.com", "a(b@x.com", "a)b@x.com",
}

func emailAll() map[string]bool {
	res := make(map[string]bool, len(emailCorpus))
	for _, in := range emailCorpus {
		res[in] = model.IsValidEmail(in)
	}
	return res
}

// --- IsValidLocale (user.go:1105) --------------------------------------------
//
// Delegates to golang.org/x/text/language.Parse, gated by a 5-character cap. The cap makes
// the reachable grammar far smaller than BCP 47, but "reachable" still needs measuring
// rather than guessing.

var localeCorpus = []string{
	"", "e", "en", "eng", "engl", "en-US", "en-us", "en_US", "EN", "En",
	"fr", "de", "ja", "zh", "pt-BR", "pt-br", "zh-CN", "zh-Ha", "sr-Cy",
	"x", "xx", "xxx", "xxxx", "xxxxx", "xxxxxx", "1", "12", "123", "en-1",
	"en-", "-en", "e-", "--", "en--", "a-b", "a_b", "en US", "en.US", "en/US",
	"i-en", "und", "mul", "zxx", "qaa", "root", "c", "C", "POSIX",
}

func localeAll() map[string]bool {
	res := make(map[string]bool, len(localeCorpus))
	for _, in := range localeCorpus {
		res[in] = model.IsValidLocale(in)
	}
	return res
}

// emailFuzzAll generates a deterministic pseudo-random corpus so the Rust port is measured
// against inputs nobody hand-picked. A hand-written corpus tests the cases its author already
// thought of; this one does not care what either implementer expected.
//
// The generator is a self-contained xorshift rather than math/rand so the output cannot drift
// with the Go release, and the fixture stays byte-stable across runs.
func emailFuzzAll() map[string]bool {
	pool := []rune("abcxyz019..@@--__[]{}!#$%&'*+/=?^`|~ \"(),:;<>\\\t\neé日")
	state := uint64(0x2545F4914F6CDD1D)
	next := func(n int) int {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		return int(state % uint64(n))
	}

	res := make(map[string]bool, 3000)
	for range 3000 {
		length := 1 + next(12)
		buf := make([]rune, length)
		for i := range buf {
			buf[i] = pool[next(len(pool))]
		}
		in := string(buf)
		res[in] = model.IsValidEmail(in)
	}
	return res
}

// --- team.go -----------------------------------------------------------------

var teamNameCorpus = []string{
	"", "a", "ab", "core-team", "Core Team", "My Team", "My  Team!!", "--core-team--",
	"Team_2024", "team2024", "!!!", "-", "--", "a-b", "a--b", "a_b",
	// reserved words: prefix hits, non-prefix misses, and the replace-all behaviour
	"admin", "administrators", "ADMIN", "adminxadmin", "xadmin", "my-admin",
	"api", "apiary", "channel", "channels", "post", "postmaster", "signup", "help",
	"boards", "playbooks", "plug", "plugins", "landing", "login", "mfa", "oauth",
	"claim", "error", "files", "system",
	// junk that survives or does not
	"a b c", "  spaced  ", "UPPER", "123", "1", "a1", "team!", "tëam", "teamé",
	repeat("a", 64), repeat("a", 65),
}

func reservedTeamNameAll() map[string]bool {
	res := make(map[string]bool, len(teamNameCorpus))
	for _, in := range teamNameCorpus {
		res[in] = model.IsReservedTeamName(in)
	}
	return res
}

func validTeamNameAll() map[string]bool {
	res := make(map[string]bool, len(teamNameCorpus))
	for _, in := range teamNameCorpus {
		res[in] = model.IsValidTeamName(in)
	}
	return res
}

// cleanTeamNameAll records "<newid>" where Go fell back to NewId(), which is random and
// cannot be pinned in a fixture. The Rust side asserts the shape for those cases.
func cleanTeamNameAll() map[string]string {
	res := make(map[string]string, len(teamNameCorpus))
	for _, in := range teamNameCorpus {
		// CleanTeamName falls back to NewId(), which is random and cannot be pinned in a
		// fixture. Two runs agreeing means the result is deterministic; disagreeing means we
		// hit the fallback. The Rust side asserts the shape for those.
		first := model.CleanTeamName(in)
		if second := model.CleanTeamName(in); second != first {
			res[in] = "<newid>"
			continue
		}
		res[in] = first
	}
	return res
}
