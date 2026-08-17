package main

// Behavioural oracle for model/channel_mentions.go, written to
// fixtures/behaviour_channel_mentions.json.
//
// The file is 96 lines and three functions, and almost all of its behaviour is one regular
// expression:
//
//	`\B~[a-zA-Z0-9\-_]+`
//
// That pattern does not transcribe. **Go's RE2 defines `\b` and `\B` over the ASCII word class**
// (`[0-9A-Za-z_]`); Rust's `regex` crate defines them over Unicode. So `é~chan` sits at a `\B`
// position in Go — `é` is not an ASCII word character — and at a word *boundary* in Rust, where
// `é` is a letter. The mention is found in one language and not the other, from an identical
// pattern string. Same family as the `\d`/`\s` divergence search_params.go hit, and the reason
// the sweeps below exist rather than a handful of happy-path cases.
//
// The four sweeps drive one varying codepoint through four positions:
//
//	prefix:  <c>~chan      — the \B decision
//	first:   ~<c>          — the start of the character class
//	middle:  ~ch<c>an      — the class in the middle of a name
//	suffix:  ~chan<c>      — where the class stops
//
// Recording all four over the same alphabet means a class transcribed as Unicode-aware (`\w`,
// or the `regex` crate's default `\d`) fails a test rather than silently accepting `~ｃhan`.
//
// The other traps, none of which survive a casual reading:
//
//  1. **Dedup is global across the whole input, and order is first-appearance.** The map is
//     allocated once outside the loop in all three functions, so a name repeated in a later
//     string is dropped, not re-emitted.
//
//  2. **All three return a nil slice, not an empty one, when nothing matches.** `var names
//     []string` is never appended to. It marshals as `null`.
//
//  3. **`ChannelMentionsFromAttachments` scans pretext, text and field *values* — not titles.**
//     Go's comment says titles are labels. It also skips a non-string field value entirely
//     rather than stringifying it, so `{"value": 123}` contributes nothing even if the number
//     were rendered with a `~` in it.
//
//  4. **`(*Post).ChannelMentionsAll`'s doc comment contradicts its body.** The comment says
//     "interactive blocks are omitted"; the call passes `OmitInteractiveBlocks: false`, which
//     includes them. Both options are recorded per post so the body is what gets ported.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeChannelMentionsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"sweep":            channelMentionSweepAll(),
		"from_strings":     channelMentionsFromStringsAll(),
		"single":           channelMentionsSingleAll(),
		"from_attachments": channelMentionsFromAttachmentsAll(),
		"post_methods":     channelMentionsPostAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_mentions.json"), append(blob, '\n'), 0o644)
}

// --- the regex sweep -------------------------------------------------------------------------

// channelMentionSweepCodepoints is every ASCII byte plus a curated non-ASCII set chosen to
// separate the ASCII word class from the Unicode one: letters that are letters only in Unicode,
// digits that are digits only in Unicode, marks, spaces of several widths, and the fullwidth
// forms of characters that ARE in the ASCII class.
func channelMentionSweepCodepoints() []rune {
	var runes []rune
	for c := rune(0); c < 128; c++ {
		runes = append(runes, c)
	}
	runes = append(runes,
		0x00E9,  // é — a Unicode letter, not an ASCII word character
		0x00C9,  // É
		0x00F1,  // ñ
		0x00DF,  // ß
		0x03A9,  // Ω Greek
		0x0434,  // д Cyrillic
		0x05D0,  // א Hebrew, RTL
		0x0627,  // ا Arabic, RTL
		0x65E5,  // 日 CJK
		0x3072,  // ひ kana
		0xD55C,  // 한 Hangul
		0x0663,  // ٣ Arabic-Indic digit THREE — Unicode Nd, not ASCII
		0x0969,  // ३ Devanagari digit
		0x2166,  // Ⅶ roman numeral — Nl, a letter that is not a plain alphanumeric
		0x00BD,  // ½ — No, a number that is not a digit
		0xFF43,  // ｃ fullwidth latin small c
		0xFF13,  // ３ fullwidth digit three
		0xFF5E,  // ～ fullwidth tilde — NOT the ASCII `~` the pattern matches
		0xFF3F,  // ＿ fullwidth low line
		0xFF0D,  // － fullwidth hyphen-minus
		0x2010,  // ‐ hyphen
		0x2013,  // – en dash
		0x2212,  // − minus sign
		0x00A0,  // NBSP
		0x2007,  // figure space
		0x200B,  // zero-width space
		0x200D,  // zero-width joiner
		0xFEFF,  // BOM / zero-width no-break space
		0x0301,  // combining acute accent — a mark, not a letter
		0x0345,  // combining ypogegrammeni — Other_Alphabetic; see the utils.go notes
		0x180E,  // Mongolian vowel separator
		0x2028,  // line separator
		0x0085,  // NEL
		0x1F600, // 😀 outside the BMP
		0x1F44D, // 👍
		0x1D400, // MATHEMATICAL BOLD CAPITAL A — a Unicode letter
	)
	return runes
}

type sweepCase struct {
	// The codepoint under test, as a number so the fixture is unambiguous about zero-width and
	// invisible characters.
	Codepoint int32 `json:"codepoint"`
	// Four templates, keyed by position. Each value is FindAllString's result on that template.
	Prefix []string `json:"prefix"`
	First  []string `json:"first"`
	Middle []string `json:"middle"`
	Suffix []string `json:"suffix"`
}

func channelMentionSweepAll() []sweepCase {
	var res []sweepCase
	for _, c := range channelMentionSweepCodepoints() {
		res = append(res, sweepCase{
			Codepoint: c,
			Prefix:    model.ChannelMentions(string(c) + "~chan"),
			First:     model.ChannelMentions(" ~" + string(c)),
			Middle:    model.ChannelMentions(" ~ch" + string(c) + "an"),
			Suffix:    model.ChannelMentions(" ~chan" + string(c)),
		})
	}
	return res
}

// --- ChannelMentionsFromStrings --------------------------------------------------------------

type mentionsCase struct {
	Name string   `json:"name"`
	In   []string `json:"in"`
	// nil is recorded as JSON null, which is the answer for "nothing matched".
	Out []string `json:"out"`
	Nil bool     `json:"nil"`
}

func channelMentionsFromStringsAll() []mentionsCase {
	corpus := []struct {
		name string
		in   []string
	}{
		{"nil_slice", nil},
		{"empty_slice", []string{}},
		{"empty_string", []string{""}},
		{"no_tilde", []string{"hello world"}},
		{"bare_tilde", []string{"~"}},
		{"tilde_then_space", []string{"~ chan"}},
		{"simple", []string{"~town-square"}},
		{"leading_space", []string{"go to ~town-square please"}},
		// \B is the whole story: a mention glued to a word is NOT found.
		{"after_letter", []string{"a~chan"}},
		{"after_digit", []string{"1~chan"}},
		{"after_underscore", []string{"_~chan"}},
		{"after_hyphen", []string{"-~chan"}},
		{"after_period", []string{".~chan"}},
		{"after_open_paren", []string{"(~chan"}},
		{"at_string_start", []string{"~chan"}},
		{"after_newline", []string{"line\n~chan"}},
		{"after_tab", []string{"a\t~chan"}},
		// Two tildes: the second is preceded by `~`, a non-word character, so \B holds.
		{"double_tilde", []string{"~~chan"}},
		{"triple_tilde", []string{"~~~chan"}},
		{"adjacent_mentions", []string{"~a~b"}},
		{"mention_then_tilde", []string{"~abc~"}},
		// The character class, at each end.
		{"all_class_chars", []string{" ~aZ09-_"}},
		{"stops_at_period", []string{" ~chan.next"}},
		{"stops_at_slash", []string{" ~chan/next"}},
		{"stops_at_colon", []string{" ~chan:"}},
		{"only_hyphen", []string{" ~-"}},
		{"only_underscore", []string{" ~_"}},
		// Dedup and ordering.
		{"repeat_same_string", []string{" ~a ~b ~a"}},
		{"repeat_across_strings", []string{" ~a ~b", " ~a ~c"}},
		{"case_sensitive_dedup", []string{" ~Chan ~chan"}},
		{"order_is_first_appearance", []string{" ~z ~y ~x ~z ~y"}},
		// The strings.Contains("~") short circuit: a string with no tilde is skipped whole.
		{"mixed_with_tildeless", []string{"nothing here", " ~a", "still nothing", " ~b"}},
		{"empty_strings_interleaved", []string{"", " ~a", ""}},
		// Non-ASCII neighbours — the divergence this whole file exists to pin.
		{"after_accented_letter", []string{"é~chan"}},
		{"after_cjk", []string{"日~chan"}},
		{"after_emoji", []string{"😀~chan"}},
		{"after_nbsp", []string{"a ~chan"}},
		{"after_zwsp", []string{"a​~chan"}},
		{"after_combining_mark", []string{"á~chan"}},
		{"non_ascii_in_name", []string{" ~chané"}},
		{"fullwidth_tilde", []string{" ～chan"}},
		{"multiline", []string{"~one\n~two\r\n~three"}},
		{"very_long_name", []string{" ~" + repeat("a", 500)}},
	}

	res := make([]mentionsCase, 0, len(corpus))
	for _, c := range corpus {
		out := model.ChannelMentionsFromStrings(c.in)
		res = append(res, mentionsCase{Name: c.name, In: c.in, Out: out, Nil: out == nil})
	}
	return res
}

// ChannelMentions is a one-line wrapper, recorded separately so a port that inlines it wrongly
// (passing the message as N strings, say) fails a test.
func channelMentionsSingleAll() []mentionsCase {
	corpus := []string{
		"",
		"~chan",
		"a~chan",
		" ~a ~b ~a",
		"~a\n~b",
		"é~chan",
	}

	res := make([]mentionsCase, 0, len(corpus))
	for _, s := range corpus {
		out := model.ChannelMentions(s)
		res = append(res, mentionsCase{Name: s, In: []string{s}, Out: out, Nil: out == nil})
	}
	return res
}

// --- ChannelMentionsFromAttachments ----------------------------------------------------------

type attachmentMentionsCase struct {
	Name string          `json:"name"`
	In   json.RawMessage `json:"in"` // the attachment slice, marshalled
	Out  []string        `json:"out"`
	Nil  bool            `json:"nil"`
	// Whether the corpus entry holds a nil attachment or a nil field, neither of which the Rust
	// types can represent ([D-033]). Recorded so those cases are skipped explicitly.
	HasNilElement bool `json:"has_nil_element"`
}

func channelMentionsFromAttachmentsAll() []attachmentMentionsCase {
	str := func(s string) any { return s }

	corpus := []struct {
		name   string
		in     []*model.MessageAttachment
		hasNil bool
	}{
		{"nil_slice", nil, false},
		{"empty_slice", []*model.MessageAttachment{}, false},
		{"nil_attachment", []*model.MessageAttachment{nil}, true},
		{"empty_attachment", []*model.MessageAttachment{{}}, false},
		{"pretext_only", []*model.MessageAttachment{{Pretext: " ~pre"}}, false},
		{"text_only", []*model.MessageAttachment{{Text: " ~txt"}}, false},
		// Titles are labels and are NOT scanned — neither the attachment's nor a field's.
		{"title_is_ignored", []*model.MessageAttachment{{Title: " ~title"}}, false},
		{"fallback_is_ignored", []*model.MessageAttachment{{Fallback: " ~fallback"}}, false},
		{"author_name_is_ignored", []*model.MessageAttachment{{AuthorName: " ~author"}}, false},
		{"footer_is_ignored", []*model.MessageAttachment{{Footer: " ~footer"}}, false},
		{"field_title_is_ignored", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Title: " ~ftitle", Value: str(" ~fvalue")}}},
		}, false},
		// Emission order within one attachment: pretext, then text, then fields in order.
		{"order_within_attachment", []*model.MessageAttachment{
			{Pretext: " ~pre", Text: " ~txt", Fields: []*model.MessageAttachmentField{
				{Value: str(" ~f1")}, {Value: str(" ~f2")},
			}},
		}, false},
		{"order_across_attachments", []*model.MessageAttachment{
			{Text: " ~second"}, {Pretext: " ~first"},
		}, false},
		{"dedup_across_attachments", []*model.MessageAttachment{
			{Text: " ~a ~b"}, {Text: " ~b ~c"},
		}, false},
		{"dedup_pretext_and_text", []*model.MessageAttachment{{Pretext: " ~a", Text: " ~a"}}, false},
		{"nil_field", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{nil, {Value: str(" ~after")}}},
		}, true},
		// A non-string field value is skipped whole, never stringified.
		{"field_value_number", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: 42}}},
		}, false},
		{"field_value_bool", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: true}}},
		}, false},
		{"field_value_nil", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: nil}}},
		}, false},
		{"field_value_slice", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: []any{" ~inslice"}}}},
		}, false},
		{"field_value_map", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: map[string]any{"k": " ~inmap"}}}},
		}, false},
		{"field_value_string_survives", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{{Value: 42}, {Value: str(" ~kept")}}},
		}, false},
		{"non_ascii_neighbour", []*model.MessageAttachment{{Text: "é~chan"}}, false},
	}

	res := make([]attachmentMentionsCase, 0, len(corpus))
	for _, c := range corpus {
		out := model.ChannelMentionsFromAttachments(c.in)
		res = append(res, attachmentMentionsCase{
			Name:          c.name,
			In:            json.RawMessage(mustMarshal(c.in)),
			Out:           out,
			Nil:           out == nil,
			HasNilElement: c.hasNil,
		})
	}
	return res
}

// --- the three Post methods ------------------------------------------------------------------

type postMentionsCase struct {
	Name string          `json:"name"`
	Post json.RawMessage `json:"post"`
	// (*Post).ChannelMentions — the message alone.
	Message []string `json:"message"`
	// (*Post).ChannelMentionsAll — AllStrings with OmitInteractiveBlocks FALSE, despite the doc
	// comment saying interactive blocks are omitted.
	All []string `json:"all"`
	// (*Post).ChannelMentionsAllWithOptions under each option value.
	WithBlocks    []string `json:"with_blocks"`
	WithoutBlocks []string `json:"without_blocks"`
}

func channelMentionsPostAll() []postMentionsCase {
	docs := []struct{ name, doc string }{
		{"empty", `{}`},
		{"message_only", `{"message":"go to ~town-square"}`},
		{"message_glued", `{"message":"a~chan"}`},
		// Attachments reach AllStrings through props, so the mention is found in both All forms.
		{"attachment_text", `{"message":" ~msg","props":{"attachments":[{"text":" ~att"}]}}`},
		{"attachment_title", `{"props":{"attachments":[{"title":" ~atitle"}]}}`},
		{"attachment_field_value", `{"props":{"attachments":[{"fields":[{"title":" ~ft","value":" ~fv"}]}]}}`},
		{"dedup_message_and_attachment", `{"message":" ~same","props":{"attachments":[{"text":" ~same"}]}}`},
		// The interactive payloads — the only place the two option values can disagree.
		{"mm_blocks", `{"message":" ~msg","props":{"mm_blocks":[{"type":"text","text":" ~inmmblock"}]}}`},
		{"blocks", `{"message":" ~msg","props":{"blocks":[{"type":"section",` +
			`"text":{"type":"mrkdwn","text":" ~inblocks"}}]}}`},
		{"cards", `{"message":" ~msg","props":{"cards":[{"body":[{"type":"TextBlock",` +
			`"text":" ~incard"}]}]}}`},
		// All three dialects at once, which is the only case where the option value changes
		// three answers rather than one.
		{"all_three_dialects", `{"message":" ~msg","props":{` +
			`"mm_blocks":[{"type":"text","text":" ~m1"}],` +
			`"blocks":[{"type":"section","text":{"type":"mrkdwn","text":" ~b1"}}],` +
			`"cards":[{"body":[{"type":"TextBlock","text":" ~c1"}]}]}}`},
		{"non_ascii", `{"message":"é~chan"}`},
	}

	res := make([]postMentionsCase, 0, len(docs))
	for _, c := range docs {
		var p model.Post
		if err := json.Unmarshal([]byte(c.doc), &p); err != nil {
			panic(err)
		}
		res = append(res, postMentionsCase{
			Name:          c.name,
			Post:          json.RawMessage(mustMarshal(&p)),
			Message:       p.ChannelMentions(),
			All:           p.ChannelMentionsAll(),
			WithBlocks:    p.ChannelMentionsAllWithOptions(model.AllStringsOptions{OmitInteractiveBlocks: false}),
			WithoutBlocks: p.ChannelMentionsAllWithOptions(model.AllStringsOptions{OmitInteractiveBlocks: true}),
		})
	}
	return res
}
