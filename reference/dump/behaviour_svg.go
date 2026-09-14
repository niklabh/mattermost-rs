package main

// Behavioural oracle for `imaging.ParseSVG` (channels/app/imaging/svg.go), written to
// fixtures/behaviour_svg.json.
//
// `UploadFileTask.preprocessImage` reads an SVG's dimensions off its root element and puts them on
// the wire as `FileInfo.width`/`height`. The function is `encoding/xml` plus `fmt.Sscan`, and both
// have edges a port would guess wrong: which prologue tokens are skipped, what a `viewBox` with the
// wrong field count does, whether `100px` scans as 100, whether a decoder error still yields the
// dimensions read so far. The corpus walks each of those; the answer recorded is the pair the
// caller uses plus whether an error came back, since the caller only logs the error.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/v8/channels/app/imaging"
)

type svgCase struct {
	Name   string `json:"name"`
	SVG    string `json:"svg"`
	Width  int    `json:"width"`
	Height int    `json:"height"`
	Err    string `json:"err"`
}

func writeSVGBehaviourFixture(outDir string) error {
	corpus := []struct{ name, svg string }{
		{"plain", `<svg width="120" height="80"></svg>`},
		{"self_closing", `<svg width="120" height="80"/>`},
		{"xmlns", `<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80"></svg>`},
		{"prologue", `<?xml version="1.0" encoding="UTF-8"?><!-- c --><!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd"><svg width="12" height="34"></svg>`},
		{"leading_whitespace", "\n\n   <svg width=\"1\" height=\"2\"></svg>"},
		{"bom", "\xef\xbb\xbf<svg width=\"1\" height=\"2\"></svg>"},
		{"viewbox_only", `<svg viewBox="0 0 300 150"></svg>`},
		{"viewbox_before_size", `<svg viewBox="0 0 300 150" width="10" height="20"></svg>`},
		{"size_before_viewbox", `<svg width="10" height="20" viewBox="0 0 300 150"></svg>`},
		{"viewbox_three_fields", `<svg viewBox="0 0 300" width="10" height="20"></svg>`},
		{"viewbox_five_fields", `<svg viewBox="0 0 300 150 7" width="10" height="20"></svg>`},
		{"viewbox_commas", `<svg viewBox="0,0,300,150"></svg>`},
		{"viewbox_floats", `<svg viewBox="0 0 300.5 150.25"></svg>`},
		{"viewbox_bad_then_size", `<svg viewBox="0 0 abc def" width="10" height="20"></svg>`},
		{"viewbox_tabs", "<svg viewBox=\"0\t0\t300\t150\"></svg>"},
		{"px_units", `<svg width="100px" height="50px"></svg>`},
		{"percent", `<svg width="100%" height="50%"></svg>`},
		{"float_size", `<svg width="100.5" height="50.9"></svg>`},
		{"exponent", `<svg width="1e2" height="5e1"></svg>`},
		{"negative", `<svg width="-100" height="50"></svg>`},
		{"hex", `<svg width="0x10" height="0x20"></svg>`},
		{"underscore", `<svg width="1_000" height="2_000"></svg>`},
		{"leading_space", `<svg width=" 100" height="	50"></svg>`},
		{"trailing_space", `<svg width="100 " height="50 "></svg>`},
		{"two_numbers", `<svg width="100 200" height="50"></svg>`},
		{"empty_value", `<svg width="" height="50"></svg>`},
		{"letters", `<svg width="abc" height="50"></svg>`},
		{"width_only", `<svg width="100"></svg>`},
		{"height_only", `<svg height="100"></svg>`},
		{"zero_width", `<svg width="0" height="100"></svg>`},
		{"no_attrs", `<svg></svg>`},
		{"namespaced_attrs", `<svg svg:width="100" svg:height="50" xmlns:svg="http://www.w3.org/2000/svg"></svg>`},
		{"single_quotes", `<svg width='100' height='50'></svg>`},
		{"entity_digits", `<svg width="&#49;&#48;" height="&#x32;0"></svg>`},
		{"entity_amp", `<svg width="1&amp;0" height="50"></svg>`},
		{"undefined_entity", `<svg width="&foo;" height="50"></svg>`},
		{"uppercase_attr", `<svg WIDTH="100" HEIGHT="50"></svg>`},
		{"first_element_not_svg", `<g width="7" height="8"><svg width="100" height="50"/></g>`},
		{"nested_after_root", `<svg><rect width="100" height="50"/></svg>`},
		{"duplicate_width", `<svg width="1" width="2" height="3"></svg>`},
		{"multiline_tag", "<svg\n  width=\"100\"\n  height=\"50\"\n>\n</svg>"},
		{"text_before_root", `hello <svg width="100" height="50"></svg>`},
		{"unterminated_tag", `<svg width="100" height="50"`},
		{"unterminated_attr", `<svg width="100 height="50"></svg>`},
		{"garbage", `not xml at all`},
		{"empty", ``},
		{"whitespace_only", "   \n"},
		{"latin1_decl", `<?xml version="1.0" encoding="ISO-8859-1"?><svg width="100" height="50"></svg>`},
		{"utf16_decl", `<?xml version="1.0" encoding="UTF-16"?><svg width="100" height="50"></svg>`},
		{"comment_only", `<!-- nothing -->`},
		{"processing_instruction_only", `<?xml version="1.0"?>`},
		{"cdata_before_root", `<![CDATA[x]]><svg width="100" height="50"></svg>`},
		{"unclosed_comment", `<!-- <svg width="100" height="50"></svg>`},
		{"attr_no_quotes", `<svg width=100 height=50></svg>`},
		{"plus_sign", `<svg width="+100" height="50"></svg>`},
		{"huge", `<svg width="99999999999999999999" height="50"></svg>`},
		{"int64_max", `<svg width="9223372036854775807" height="50"></svg>`},
		{"octal_like", `<svg width="0100" height="050"></svg>`},
		{"newline_in_value", "<svg width=\"100\n\" height=\"50\"></svg>"},
		{"png_bytes", "\x89PNG\r\n\x1a\n"},
		{"doctype_with_subset", `<!DOCTYPE svg [<!ENTITY w "100">]><svg width="&w;" height="50"></svg>`},
		{"root_after_nonascii", `<svg width="１００" height="50"></svg>`},
		{"null_byte", "<svg width=\"100\x00\" height=\"50\"></svg>"},
	}

	cases := make([]svgCase, 0, len(corpus))
	for _, c := range corpus {
		info, err := imaging.ParseSVG(strings.NewReader(c.svg))
		out := svgCase{Name: c.name, SVG: c.svg, Width: info.Width, Height: info.Height}
		if err != nil {
			out.Err = err.Error()
		}
		cases = append(cases, out)
	}
	data, err := json.MarshalIndent(map[string]any{"parse_svg": cases}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_svg.json"), append(data, '\n'), 0o644)
}
