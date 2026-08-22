package main

// Behavioural oracle for model/channel_sidebar.go, written to
// fixtures/behaviour_sidebar_category.json.
//
// The file declares one function with a branch in it, IsValidCategoryId, and the branch is a
// regexp whose behaviour is not what a reader assumes:
//
//  1. **The pattern is unanchored.** `regexp.MustCompile("(favorites|channels|direct_messages)_
//     [a-z0-9]{26}_[a-z0-9]{26}")` is used with MatchString, which asks whether the pattern occurs
//     ANYWHERE in the string. So a default category id with arbitrary text glued to either end is
//     accepted, and a Rust port that reaches for `^...$` — the obvious translation — rejects
//     inputs Go admits. Every `*_prefix` / `*_suffix` case below exists to pin that.
//
//  2. **The two id halves are `[a-z0-9]`, but `IsValidId` is not.** `model.IsValidId` accepts
//     Go's unicode letter and number classes, so an upper-case 26-character string is valid by
//     the FIRST branch while the same characters inside the default-category pattern are not.
//     The two branches therefore disagree about case, which no single character class expresses.
//
//  3. **`custom` and `managed` are category types but not in the alternation.** A custom
//     category always carries a real 26-character id, so the pattern never needs them — but the
//     constants sit five lines above the regexp and inviting them in is a one-word edit.
//
// Answers are computed by Go here rather than reasoned about in the port.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

// Two fixed 26-character lower-case halves, so a reader can count them.
const (
	lowerA  = "abcdefghijklmnopqrstuvwxyz"  // 26 letters
	lowerB  = "0123456789abcdefghijklmnop"  // 26 mixed
	upper   = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"  // 26, upper case
	real26  = "y9i4er48tt8bukijy7i3u5y9ar"  // a NewId-shaped id
	long27  = "abcdefghijklmnopqrstuvwxyza" // 27 lower-case
	short25 = "abcdefghijklmnopqrstuvwxy"   // 25 lower-case
)

func sidebarCategoryIdCases() []map[string]any {
	inputs := []string{
		"",
		real26,
		upper,
		short25,
		long27,
		// The three default-category shapes.
		"favorites_" + lowerA + "_" + lowerB,
		"channels_" + lowerA + "_" + lowerB,
		"direct_messages_" + lowerA + "_" + lowerB,
		// Types that exist but are absent from the alternation.
		"custom_" + lowerA + "_" + lowerB,
		"managed_" + lowerA + "_" + lowerB,
		// Unanchored: text before, after, and both.
		"zz" + "favorites_" + lowerA + "_" + lowerB,
		"favorites_" + lowerA + "_" + lowerB + "zz",
		"zz" + "channels_" + lowerA + "_" + lowerB + "!!",
		"/" + "direct_messages_" + lowerA + "_" + lowerB + "/",
		// Wrong-length halves.
		"favorites_" + short25 + "_" + lowerB,
		"favorites_" + lowerA + "_" + short25,
		"favorites_" + long27 + "_" + lowerB,
		// A 27-character SECOND half still matches: the pattern needs 26 and stops there.
		"favorites_" + lowerA + "_" + long27,
		// Case: the halves are [a-z0-9] only.
		"favorites_" + upper + "_" + lowerB,
		"FAVORITES_" + lowerA + "_" + lowerB,
		// Separator errors.
		"favorites" + lowerA + "_" + lowerB,
		"favorites_" + lowerA + lowerB,
		// Substrings of the alternation.
		"messages_" + lowerA + "_" + lowerB,
		"channel_" + lowerA + "_" + lowerB,
	}

	cases := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		cases = append(cases, map[string]any{
			"input":                in,
			"is_valid_category_id": model.IsValidCategoryId(in),
			"is_valid_id":          model.IsValidId(in),
		})
	}
	return cases
}

// The constants, emitted rather than transcribed. behaviour_version.go's note applies: a
// transcribed constant drifts silently, and these five are the `type` field of every category on
// the wire.
func sidebarCategoryConstants() map[string]any {
	return map[string]any{
		"SidebarCategoryChannels":          string(model.SidebarCategoryChannels),
		"SidebarCategoryDirectMessages":    string(model.SidebarCategoryDirectMessages),
		"SidebarCategoryFavorites":         string(model.SidebarCategoryFavorites),
		"SidebarCategoryCustom":            string(model.SidebarCategoryCustom),
		"SidebarCategoryManaged":           string(model.SidebarCategoryManaged),
		"MinimalSidebarSortDistance":       model.MinimalSidebarSortDistance,
		"DefaultSidebarSortOrderFavorites": model.DefaultSidebarSortOrderFavorites,
		"DefaultSidebarSortOrderChannels":  model.DefaultSidebarSortOrderChannels,
		"DefaultSidebarSortOrderDMs":       model.DefaultSidebarSortOrderDMs,
		"SidebarCategorySortDefault":       string(model.SidebarCategorySortDefault),
		"SidebarCategorySortManual":        string(model.SidebarCategorySortManual),
		"SidebarCategorySortRecent":        string(model.SidebarCategorySortRecent),
		"SidebarCategorySortAlphabetical":  string(model.SidebarCategorySortAlphabetical),
		"ManagedCategoryPropertyGroupName": model.ManagedCategoryPropertyGroupName,
		"ManagedCategoryPropertyFieldName": model.ManagedCategoryPropertyFieldName,
	}
}

// A nil `Channels` slice marshals as `null`, not `[]` — the field carries no omitempty. The
// store always builds `make([]string, 0)`, so `[]` is what the three read routes emit; this
// records both so the port models the difference instead of choosing one.
func sidebarCategoryNilShapes() map[string]any {
	nilChannels := model.SidebarCategoryWithChannels{
		SidebarCategory: model.SidebarCategory{Id: real26},
	}
	emptyChannels := model.SidebarCategoryWithChannels{
		SidebarCategory: model.SidebarCategory{Id: real26},
		Channels:        []string{},
	}
	nilOrdered := model.OrderedSidebarCategories{}
	emptyOrdered := model.OrderedSidebarCategories{
		Categories: model.SidebarCategoriesWithChannels{},
		Order:      model.SidebarCategoryOrder{},
	}

	marshal := func(v any) json.RawMessage {
		blob, err := json.Marshal(v)
		if err != nil {
			return json.RawMessage(`"<marshal failed>"`)
		}
		return json.RawMessage(blob)
	}

	return map[string]any{
		"category_nil_channels":   marshal(nilChannels),
		"category_empty_channels": marshal(emptyChannels),
		"ordered_nil":             marshal(nilOrdered),
		"ordered_empty":           marshal(emptyOrdered),
		// ChannelIds() is a plain accessor; recorded so "it returns Channels, not a copy or a
		// sorted view" is asserted rather than assumed.
		"channel_ids_of_nil":   marshal(nilChannels.ChannelIds()),
		"channel_ids_of_empty": marshal(emptyChannels.ChannelIds()),
	}
}

func writeSidebarCategoryBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":    sidebarCategoryConstants(),
		"category_ids": sidebarCategoryIdCases(),
		"nil_shapes":   sidebarCategoryNilShapes(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_sidebar_category.json"), append(blob, '\n'), 0o644)
}
