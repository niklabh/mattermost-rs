package main

// Behavioural oracle for model/post_interactive_blocks.go, written to
// fixtures/behaviour_post_interactive_blocks.json.
//
// The file is 41 functions walking untyped JSON trees — mm_blocks, Block Kit blocks and
// Adaptive Cards — for two purposes: human-readable text (the half of Post.AllStrings that was
// deferred) and image URLs. Neither walker has a type to round-trip, so the oracle is the only
// thing that can distinguish a faithful port from a plausible one.
//
// Both are driven through their exported callers, since every walker in the file is unexported:
//
//	human_strings -> (*Post).AllStrings(AllStringsOptions{OmitInteractiveBlocks: false})
//	image_urls    -> (*Post).InteractiveBlocksImageURLs(mmBlocksEnabled)
//
// Three things need Go's own answer:
//
//  1. **The two walkers disagree about column_set.** The human-strings walker hands the whole
//     `items` array to the block walker; the image walker hands it **each element**, which the
//     block walker then re-tests as an array. So an image inside a column reaches the output
//     only when `items` is an array *of arrays*. Recorded both ways.
//
//  2. **Which container types recurse, and which text keys are read, differ per dialect.**
//     mm_blocks reads `text` off a `text` block; Block Kit reads `text.text` off a `section` and
//     a bare `text` off a `markdown`; Adaptive Cards read `text` off a `TextBlock`. Every
//     mismatch (a Block Kit `text` string where a map is expected, and the reverse) is a silent
//     no-op rather than an error, so the corpus drives each one.
//
//  3. **mmBlocksEnabled gates all three block dialects, not just mm_blocks** — and attachment
//     image URLs are collected regardless. The flag is recorded under both values.
//
// Not covered here, and deliberately: everything downstream of `appendMmactionIDsFromText`,
// which calls `markdown.Inspect`. See [D-044].

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostInteractiveBlocksBehaviourFixture(outDir string) error {
	out := map[string]any{
		"human_strings": interactiveHumanStringsAll(),
		"image_urls":    interactiveImageURLsAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_interactive_blocks.json"), append(blob, '\n'), 0o644)
}

// --- human strings ----------------------------------------------------------------------------

type interactiveHumanStringsCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Both option values, so the port can assert that the interactive half is exactly the
	// difference between them.
	Omitting []string `json:"omitting"`
	Full     []string `json:"full"`
}

func interactiveHumanStringsAll() []interactiveHumanStringsCase {
	cases := []struct{ name, post string }{
		// --- the prop itself ------------------------------------------------------------
		{"no_props", `{"message":"m"}`},
		{"mm_blocks_null", `{"props":{"mm_blocks":null}}`},
		{"mm_blocks_object", `{"props":{"mm_blocks":{"type":"text","text":"x"}}}`},
		{"mm_blocks_string", `{"props":{"mm_blocks":"x"}}`},
		{"mm_blocks_empty", `{"props":{"mm_blocks":[]}}`},
		{"mm_blocks_element_not_a_map", `{"props":{"mm_blocks":["x",1,null,[]]}}`},

		// --- mm_blocks ------------------------------------------------------------------
		{"mm_text", `{"props":{"mm_blocks":[{"type":"text","text":"hello"}]}}`},
		{"mm_text_blank", `{"props":{"mm_blocks":[{"type":"text","text":"   "}]}}`},
		{"mm_text_padded", `{"props":{"mm_blocks":[{"type":"text","text":"  hello  "}]}}`},
		{"mm_text_not_a_string", `{"props":{"mm_blocks":[{"type":"text","text":42}]}}`},
		{"mm_text_missing", `{"props":{"mm_blocks":[{"type":"text"}]}}`},
		{"mm_type_missing", `{"props":{"mm_blocks":[{"text":"hello"}]}}`},
		{"mm_type_unknown", `{"props":{"mm_blocks":[{"type":"nope","text":"hello"}]}}`},
		{"mm_type_not_a_string", `{"props":{"mm_blocks":[{"type":7,"text":"hello"}]}}`},
		{"mm_container", `{"props":{"mm_blocks":[{"type":"container","content":[{"type":"text","text":"in"}]}]}}`},
		{"mm_container_content_missing", `{"props":{"mm_blocks":[{"type":"container"}]}}`},
		{"mm_container_content_object", `{"props":{"mm_blocks":[{"type":"container","content":{"type":"text","text":"in"}}]}}`},
		{"mm_container_nested", `{"props":{"mm_blocks":[{"type":"container","content":[` +
			`{"type":"container","content":[{"type":"text","text":"deep"}]}]}]}}`},
		{"mm_collapsible_order", `{"props":{"mm_blocks":[{"type":"collapsible",` +
			`"header":[{"type":"text","text":"h"}],"content":[{"type":"text","text":"c"}]}]}}`},
		{"mm_collapsible_header_only", `{"props":{"mm_blocks":[{"type":"collapsible",` +
			`"header":[{"type":"text","text":"h"}]}]}}`},
		{"mm_column_set", `{"props":{"mm_blocks":[{"type":"column_set","columns":[` +
			`{"items":[{"type":"text","text":"c1"}]},{"items":[{"type":"text","text":"c2"}]}]}]}}`},
		{"mm_column_set_no_type_needed", `{"props":{"mm_blocks":[{"type":"column_set","columns":[` +
			`{"type":"column","items":[{"type":"text","text":"c1"}]}]}]}}`},
		{"mm_column_set_items_missing", `{"props":{"mm_blocks":[{"type":"column_set","columns":[{}]}]}}`},
		{"mm_column_set_column_not_a_map", `{"props":{"mm_blocks":[{"type":"column_set","columns":["x"]}]}}`},
		{"mm_column_set_columns_object", `{"props":{"mm_blocks":[{"type":"column_set","columns":{}}]}}`},
		{"mm_two_blocks_order", `{"props":{"mm_blocks":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}`},

		// --- Block Kit ------------------------------------------------------------------
		{"kit_markdown", `{"props":{"blocks":[{"type":"markdown","text":"md"}]}}`},
		{"kit_markdown_text_is_a_map", `{"props":{"blocks":[{"type":"markdown","text":{"text":"md"}}]}}`},
		{"kit_section_text", `{"props":{"blocks":[{"type":"section","text":{"type":"mrkdwn","text":"sec"}}]}}`},
		{"kit_section_text_is_a_string", `{"props":{"blocks":[{"type":"section","text":"sec"}]}}`},
		{"kit_section_text_inner_missing", `{"props":{"blocks":[{"type":"section","text":{"type":"mrkdwn"}}]}}`},
		{"kit_section_fields", `{"props":{"blocks":[{"type":"section","fields":[` +
			`{"type":"mrkdwn","text":"f1"},{"type":"mrkdwn","text":"f2"}]}]}}`},
		{"kit_section_text_and_fields_order", `{"props":{"blocks":[{"type":"section",` +
			`"text":{"text":"sec"},"fields":[{"text":"f1"}]}]}}`},
		{"kit_section_field_not_a_map", `{"props":{"blocks":[{"type":"section","fields":["f1"]}]}}`},
		{"kit_section_field_text_not_a_string", `{"props":{"blocks":[{"type":"section","fields":[{"text":7}]}]}}`},
		{"kit_header", `{"props":{"blocks":[{"type":"header","text":{"type":"plain_text","text":"head"}}]}}`},
		{"kit_actions_ignored", `{"props":{"blocks":[{"type":"actions","elements":[` +
			`{"type":"button","text":{"text":"press"},"action_id":"a"}]}]}}`},
		{"kit_image_block_alt_ignored", `{"props":{"blocks":[{"type":"image","image_url":"https://x/i.png","alt_text":"alt"}]}}`},
		{"kit_unknown_type", `{"props":{"blocks":[{"type":"nope","text":"x"}]}}`},

		// --- Adaptive Cards -------------------------------------------------------------
		{"card_text_block", `{"props":{"cards":[{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"t"}]}]}}`},
		{"card_body_missing", `{"props":{"cards":[{"type":"AdaptiveCard"}]}}`},
		{"card_body_object", `{"props":{"cards":[{"body":{"type":"TextBlock","text":"t"}}]}}`},
		{"card_container", `{"props":{"cards":[{"body":[{"type":"Container","items":[` +
			`{"type":"TextBlock","text":"in"}]}]}]}}`},
		{"card_container_nested", `{"props":{"cards":[{"body":[{"type":"Container","items":[` +
			`{"type":"Container","items":[{"type":"TextBlock","text":"deep"}]}]}]}]}}`},
		{"card_column_set", `{"props":{"cards":[{"body":[{"type":"ColumnSet","columns":[` +
			`{"items":[{"type":"TextBlock","text":"c1"}]},{"items":[{"type":"TextBlock","text":"c2"}]}]}]}]}}`},
		{"card_column_set_items_missing", `{"props":{"cards":[{"body":[{"type":"ColumnSet","columns":[{}]}]}]}}`},
		{"card_action_set_ignored", `{"props":{"cards":[{"body":[{"type":"ActionSet","actions":[` +
			`{"type":"Action.Submit","id":"a","title":"press"}]}]}]}}`},
		{"card_top_level_actions_ignored", `{"props":{"cards":[{"body":[{"type":"TextBlock","text":"t"}],` +
			`"actions":[{"type":"Action.Submit","id":"a","title":"press"}]}]}}`},
		{"card_unknown_item_type", `{"props":{"cards":[{"body":[{"type":"Nope","text":"t"}]}]}}`},

		// --- ordering across sources ----------------------------------------------------
		{"all_three_dialects", `{"message":"msg","props":{` +
			`"cards":[{"body":[{"type":"TextBlock","text":"card"}]}],` +
			`"blocks":[{"type":"markdown","text":"kit"}],` +
			`"mm_blocks":[{"type":"text","text":"mm"}]}}`},
		{"attachments_come_before_blocks", `{"message":"msg","props":{` +
			`"attachments":[{"title":"att"}],` +
			`"mm_blocks":[{"type":"text","text":"mm"}]}}`},
	}

	res := make([]interactiveHumanStringsCase, 0, len(cases))
	for _, c := range cases {
		res = append(res, interactiveHumanStringsCase{
			Name:     c.name,
			Post:     c.post,
			Omitting: postFromJSON(c.post).AllStrings(model.AllStringsOptions{OmitInteractiveBlocks: true}),
			Full:     postFromJSON(c.post).AllStrings(model.AllStringsOptions{OmitInteractiveBlocks: false}),
		})
	}
	return res
}

// --- image URLs -------------------------------------------------------------------------------

type interactiveImageURLsCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Recorded under both flag values: mmBlocksEnabled gates all three block dialects and
	// nothing else, which is not what the parameter name suggests.
	Enabled  []string `json:"enabled"`
	Disabled []string `json:"disabled"`
}

func interactiveImageURLsAll() []interactiveImageURLsCase {
	cases := []struct{ name, post string }{
		{"no_props", `{"message":"m"}`},
		{"empty_props", `{"props":{}}`},

		// --- mm_blocks ------------------------------------------------------------------
		{"mm_image", `{"props":{"mm_blocks":[{"type":"image","url":"https://x/a.png"}]}}`},
		{"mm_image_url_missing", `{"props":{"mm_blocks":[{"type":"image"}]}}`},
		{"mm_image_url_not_a_string", `{"props":{"mm_blocks":[{"type":"image","url":7}]}}`},
		{"mm_image_url_empty", `{"props":{"mm_blocks":[{"type":"image","url":""}]}}`},
		{"mm_image_relative", `{"props":{"mm_blocks":[{"type":"image","url":"/plugins/x/img"}]}}`},
		{"mm_container", `{"props":{"mm_blocks":[{"type":"container","content":[` +
			`{"type":"image","url":"https://x/a.png"}]}]}}`},
		{"mm_collapsible_order", `{"props":{"mm_blocks":[{"type":"collapsible",` +
			`"header":[{"type":"image","url":"https://x/h.png"}],` +
			`"content":[{"type":"image","url":"https://x/c.png"}]}]}}`},
		// The image walker passes each *item* to the array walker, so a column's items must be
		// an array of arrays for anything to come out. Both shapes are recorded.
		{"mm_column_set_flat_items", `{"props":{"mm_blocks":[{"type":"column_set","columns":[` +
			`{"items":[{"type":"image","url":"https://x/a.png"}]}]}]}}`},
		{"mm_column_set_nested_items", `{"props":{"mm_blocks":[{"type":"column_set","columns":[` +
			`{"items":[[{"type":"image","url":"https://x/a.png"}]]}]}]}}`},
		{"mm_text_block_ignored", `{"props":{"mm_blocks":[{"type":"text","text":"![a](https://x/md.png)"}]}}`},
		{"mm_two_images_order", `{"props":{"mm_blocks":[{"type":"image","url":"https://x/1.png"},` +
			`{"type":"image","url":"https://x/2.png"}]}}`},

		// --- Block Kit ------------------------------------------------------------------
		{"kit_image_block", `{"props":{"blocks":[{"type":"image","image_url":"https://x/k.png"}]}}`},
		{"kit_image_block_url_key", `{"props":{"blocks":[{"type":"image","url":"https://x/k.png"}]}}`},
		{"kit_section_accessory_image", `{"props":{"blocks":[{"type":"section",` +
			`"text":{"text":"t"},"accessory":{"type":"image","image_url":"https://x/acc.png"}}]}}`},
		{"kit_section_accessory_not_an_image", `{"props":{"blocks":[{"type":"section",` +
			`"accessory":{"type":"button","image_url":"https://x/acc.png"}},` +
			`{"type":"image","image_url":"https://x/after.png"}]}}`},
		{"kit_section_without_accessory", `{"props":{"blocks":[{"type":"section","text":{"text":"t"}},` +
			`{"type":"image","image_url":"https://x/after.png"}]}}`},

		// --- Adaptive Cards -------------------------------------------------------------
		{"card_image", `{"props":{"cards":[{"body":[{"type":"Image","url":"https://x/c.png"}]}]}}`},
		{"card_image_url_key_wrong", `{"props":{"cards":[{"body":[{"type":"Image","image_url":"https://x/c.png"}]}]}}`},
		{"card_container", `{"props":{"cards":[{"body":[{"type":"Container","items":[` +
			`{"type":"Image","url":"https://x/c.png"}]}]}]}}`},
		{"card_column_set", `{"props":{"cards":[{"body":[{"type":"ColumnSet","columns":[` +
			`{"items":[{"type":"Image","url":"https://x/c1.png"}]}]}]}]}}`},

		// --- attachments ----------------------------------------------------------------
		{"attachment_all_four", `{"props":{"attachments":[{"image_url":"https://x/i.png",` +
			`"thumb_url":"https://x/t.png","author_icon":"https://x/a.png","footer_icon":"https://x/f.png"}]}}`},
		{"attachment_empty_strings_skipped", `{"props":{"attachments":[{"image_url":"","thumb_url":"https://x/t.png"}]}}`},
		{"attachment_malformed_dropped", `{"props":{"attachments":[{"image_url":123}]}}`},
		{"attachment_two", `{"props":{"attachments":[{"image_url":"https://x/1.png"},{"image_url":"https://x/2.png"}]}}`},

		// --- combined -------------------------------------------------------------------
		{"blocks_then_attachments", `{"props":{"attachments":[{"image_url":"https://x/att.png"}],` +
			`"mm_blocks":[{"type":"image","url":"https://x/mm.png"}],` +
			`"blocks":[{"type":"image","image_url":"https://x/kit.png"}],` +
			`"cards":[{"body":[{"type":"Image","url":"https://x/card.png"}]}]}}`},
	}

	res := make([]interactiveImageURLsCase, 0, len(cases))
	for _, c := range cases {
		res = append(res, interactiveImageURLsCase{
			Name:     c.name,
			Post:     c.post,
			Enabled:  postFromJSON(c.post).InteractiveBlocksImageURLs(true),
			Disabled: postFromJSON(c.post).InteractiveBlocksImageURLs(false),
		})
	}
	return res
}
