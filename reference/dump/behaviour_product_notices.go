package main

// Behavioural oracle for model/product_notices.go, written to
// fixtures/behaviour_product_notices.json.
//
// Four `Matches` methods, four small state machines, and **they disagree about what an unknown
// value means**. That is the content of this file.
//
//	NoticeAudience.Matches      unknown -> false   (switch with no default; falls to `return false`)
//	NoticeInstanceType.Matches  unknown -> TRUE    (three ifs, then `return true`)
//	NoticeClientType.Matches    unknown -> exact equality only
//	NoticeSKU.Matches           unknown -> exact equality only
//
// So an audience nobody recognises hides the notice and an instance type nobody recognises shows
// it. A port that normalises the four into one shape — an enum with a uniform fallback, say —
// silently changes who sees a notice, in opposite directions depending on the field. Every value,
// known and unknown, is driven through each.
//
// # NoticeClientTypeFromString rejects two of its own constants
//
// The function accepts "web", "mobile-ios", "mobile-android" and "desktop" — but **not** "mobile"
// or "all", both of which are declared `NoticeClientType` constants. On any other input it
// returns an error *and* `NoticeClientTypeAll`, so the value is meaningful even in the failure
// case and a caller that ignores the error gets "all" rather than a zero value.
//
// # NoticeMessage embeds NoticeMessageInternal
//
// An anonymous field, so Go inlines its keys and emits them **first**. serde's `flatten` emits
// them last, which is [D-067] again — the port needs a hand-written Serialize, as ScheduledPost
// does.
//
// # ProductNoticeViewState has no json tags at all
//
// So its wire keys are the Go field names in PascalCase, the `wrangler.go` shape.
//
// Determinism: fixed values only. See [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeProductNoticesBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":        pnConstants(),
		"keys":             pnKeys(),
		"wire":             pnWireAll(),
		"audience_matches": pnAudienceAll(),
		"client_matches":   pnClientTypeAll(),
		"instance_matches": pnInstanceTypeAll(),
		"sku_matches":      pnSKUAll(),
		"client_from_str":  pnClientFromStringAll(),
		"admin_only":       pnAdminOnlyAll(),
		"nil_receivers":    pnNilReceiverAll(),
		"round_trip":       pnRoundTripAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_product_notices.json"), append(blob, '\n'), 0o644)
}

func pnConstants() map[string]any {
	return map[string]any{
		"NoticeAudienceAll":       string(model.NoticeAudienceAll),
		"NoticeAudienceMember":    string(model.NoticeAudienceMember),
		"NoticeAudienceSysadmin":  string(model.NoticeAudienceSysadmin),
		"NoticeAudienceTeamAdmin": string(model.NoticeAudienceTeamAdmin),

		"NoticeClientTypeAll":           string(model.NoticeClientTypeAll),
		"NoticeClientTypeDesktop":       string(model.NoticeClientTypeDesktop),
		"NoticeClientTypeMobile":        string(model.NoticeClientTypeMobile),
		"NoticeClientTypeMobileAndroid": string(model.NoticeClientTypeMobileAndroid),
		"NoticeClientTypeMobileIos":     string(model.NoticeClientTypeMobileIos),
		"NoticeClientTypeWeb":           string(model.NoticeClientTypeWeb),

		"NoticeInstanceTypeBoth":   string(model.NoticeInstanceTypeBoth),
		"NoticeInstanceTypeCloud":  string(model.NoticeInstanceTypeCloud),
		"NoticeInstanceTypeOnPrem": string(model.NoticeInstanceTypeOnPrem),

		"NoticeSKUE0":   string(model.NoticeSKUE0),
		"NoticeSKUE10":  string(model.NoticeSKUE10),
		"NoticeSKUE20":  string(model.NoticeSKUE20),
		"NoticeSKUAll":  string(model.NoticeSKUAll),
		"NoticeSKUTeam": string(model.NoticeSKUTeam),

		"URL": string(model.URL),
	}
}

func pnKeys() map[string]any {
	return map[string]any{
		"notice":           expectedKeys(reflect.TypeOf(model.ProductNotice{})),
		"conditions":       expectedKeys(reflect.TypeOf(model.Conditions{})),
		"message_internal": expectedKeys(reflect.TypeOf(model.NoticeMessageInternal{})),
		"view_state":       expectedKeys(reflect.TypeOf(model.ProductNoticeViewState{})),
		"external_dep":     expectedKeys(reflect.TypeOf(model.ExternalDependency{})),
	}
}

// --- the wire format -------------------------------------------------------------------------

func pnStr(v string) *string { return &v }
func pnBool(v bool) *bool    { return &v }
func pnI64(v int64) *int64   { return &v }

func pnWireAll() []map[string]any {
	audience := model.NoticeAudienceSysadmin
	clientType := model.NoticeClientTypeMobile
	instanceType := model.NoticeInstanceTypeCloud
	sku := model.NoticeSKUE20
	action := model.URL

	fullInternal := model.NoticeMessageInternal{
		Action:      &action,
		ActionParam: pnStr("/console/some-page"),
		ActionText:  pnStr("Go"),
		Description: "the description",
		Image:       pnStr("https://example.com/i.png"),
		Title:       "the title",
	}

	out := make([]map[string]any, 0)
	add := func(name string, v any) {
		blob, err := json.Marshal(v)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": name, "json": string(blob)})
	}

	// Every field of Conditions is omitempty, so the zero value is `{}` — and ProductNotice's
	// `conditions` key is NOT omitempty, so it is present holding that `{}`.
	add("notice_zero", &model.ProductNotice{})
	add("notice_full", &model.ProductNotice{
		ID: "notice-1",
		Conditions: model.Conditions{
			Audience:              &audience,
			ClientType:            &clientType,
			DesktopVersion:        []string{">=1.2.3"},
			DisplayDate:           pnStr(">= 2020-03-01T00:00:00Z"),
			InstanceType:          &instanceType,
			MobileVersion:         []string{"<v5.19"},
			NumberOfPosts:         pnI64(100),
			NumberOfUsers:         pnI64(10),
			ServerConfig:          map[string]any{"PluginSettings.Enable": true},
			ServerVersion:         []string{">=5.20"},
			Sku:                   &sku,
			UserConfig:            map[string]any{"new_sidebar.disabled": true},
			DeprecatingDependency: &model.ExternalDependency{Name: "postgres", MinimumVersion: "11"},
		},
		LocalizedMessages: map[string]model.NoticeMessageInternal{
			"en": {Description: "English description", Title: "English"},
		},
		Repeatable: pnBool(true),
	})
	// Repeatable is the only omitempty field on ProductNotice; a pointer to false is not nil.
	add("notice_repeatable_false", &model.ProductNotice{
		ID:         "notice-2",
		Repeatable: pnBool(false),
	})

	// The embed: Go inlines NoticeMessageInternal's keys and emits them FIRST.
	add("message_full", &model.NoticeMessage{
		NoticeMessageInternal: fullInternal,
		ID:                    "notice-1",
		SysAdminOnly:          true,
		TeamAdminOnly:         false,
	})
	add("message_zero", &model.NoticeMessage{})
	add("message_internal_zero", &model.NoticeMessageInternal{})
	add("message_internal_full", &fullInternal)

	// No json tags at all: PascalCase field names.
	add("view_state", &model.ProductNoticeViewState{
		UserId:    "y9i4er48tt8bukijy7i3u5y9ar",
		NoticeId:  "notice-1",
		Viewed:    3,
		Timestamp: 1700000000000,
	})
	add("view_state_zero", &model.ProductNoticeViewState{})
	add("external_dependency", &model.ExternalDependency{Name: "postgres", MinimumVersion: "11"})

	// The list types.
	notices := model.ProductNotices{{ID: "a"}, {ID: "b"}}
	add("notices_list", &notices)
	empty := model.ProductNotices{}
	add("notices_list_empty", &empty)
	messages := model.NoticeMessages{{ID: "a"}, {ID: "b"}}
	add("messages_list", &messages)

	return out
}

// --- the four Matches state machines ---------------------------------------------------------

// Every audience value crossed with every (sysAdmin, teamAdmin) pair, plus an unrecognised one.
func pnAudienceAll() []map[string]any {
	audiences := []model.NoticeAudience{
		model.NoticeAudienceAll,
		model.NoticeAudienceMember,
		model.NoticeAudienceSysadmin,
		model.NoticeAudienceTeamAdmin,
		"",         // the zero value is not one of the four
		"unknown",  // and neither is anything else
		"SYSADMIN", // case matters
	}
	flags := []struct{ sysAdmin, teamAdmin bool }{
		{false, false}, {true, false}, {false, true}, {true, true},
	}

	out := make([]map[string]any, 0)
	for _, a := range audiences {
		audience := a
		for _, f := range flags {
			out = append(out, map[string]any{
				"audience":   string(a),
				"sys_admin":  f.sysAdmin,
				"team_admin": f.teamAdmin,
				"matches":    audience.Matches(f.sysAdmin, f.teamAdmin),
			})
		}
	}
	return out
}

// Every client type crossed with every other, including the two the `mobile` alias covers.
func pnClientTypeAll() []map[string]any {
	types := []model.NoticeClientType{
		model.NoticeClientTypeAll,
		model.NoticeClientTypeDesktop,
		model.NoticeClientTypeMobile,
		model.NoticeClientTypeMobileAndroid,
		model.NoticeClientTypeMobileIos,
		model.NoticeClientTypeWeb,
		"",
		"unknown",
	}

	out := make([]map[string]any, 0)
	for _, c := range types {
		self := c
		for _, other := range types {
			out = append(out, map[string]any{
				"client":  string(c),
				"other":   string(other),
				"matches": self.Matches(other),
			})
		}
	}
	return out
}

func pnInstanceTypeAll() []map[string]any {
	types := []model.NoticeInstanceType{
		model.NoticeInstanceTypeBoth,
		model.NoticeInstanceTypeCloud,
		model.NoticeInstanceTypeOnPrem,
		"",
		"unknown",
	}

	out := make([]map[string]any, 0)
	for _, t := range types {
		instance := t
		for _, isCloud := range []bool{false, true} {
			out = append(out, map[string]any{
				"instance": string(t),
				"is_cloud": isCloud,
				"matches":  instance.Matches(isCloud),
			})
		}
	}
	return out
}

func pnSKUAll() []map[string]any {
	skus := []model.NoticeSKU{
		model.NoticeSKUAll,
		model.NoticeSKUE0,
		model.NoticeSKUE10,
		model.NoticeSKUE20,
		model.NoticeSKUTeam,
		"",
		"unknown",
	}
	// The argument is a plain string, not a NoticeSKU — the licence SKU, where "" means none.
	others := []string{"", "e0", "e10", "e20", "team", "all", "unknown"}

	out := make([]map[string]any, 0)
	for _, s := range skus {
		sku := s
		for _, other := range others {
			out = append(out, map[string]any{
				"sku":     string(s),
				"other":   other,
				"matches": sku.Matches(other),
			})
		}
	}
	return out
}

// --- NoticeClientTypeFromString --------------------------------------------------------------

func pnClientFromStringAll() []map[string]any {
	inputs := []string{
		"web", "mobile-ios", "mobile-android", "desktop",
		// Declared constants the function does NOT accept.
		"mobile", "all",
		// Anything else.
		"", "Web", "WEB", "unknown", " web",
	}

	out := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		value, err := model.NoticeClientTypeFromString(in)
		entry := map[string]any{
			"input": in,
			"value": string(value),
			"ok":    err == nil,
		}
		if err != nil {
			entry["error"] = err.Error()
		}
		out = append(out, entry)
	}
	return out
}

// --- SysAdminOnly / TeamAdminOnly --------------------------------------------------------------

func pnAdminOnlyAll() []map[string]any {
	audiences := []*model.NoticeAudience{
		nil,
		model.NewNoticeAudience(model.NoticeAudienceAll),
		model.NewNoticeAudience(model.NoticeAudienceMember),
		model.NewNoticeAudience(model.NoticeAudienceSysadmin),
		model.NewNoticeAudience(model.NoticeAudienceTeamAdmin),
		model.NewNoticeAudience("unknown"),
	}

	out := make([]map[string]any, 0, len(audiences))
	for _, a := range audiences {
		notice := model.ProductNotice{Conditions: model.Conditions{Audience: a}}
		name := "nil"
		if a != nil {
			name = string(*a)
		}
		out = append(out, map[string]any{
			"audience":        name,
			"sys_admin_only":  notice.SysAdminOnly(),
			"team_admin_only": notice.TeamAdminOnly(),
		})
	}
	return out
}

// --- nil receivers -----------------------------------------------------------------------------

// Every Matches method has a pointer receiver and dereferences it immediately, so a nil pointer
// panics. Probed rather than assumed, because the fields holding these are all pointers and a
// caller reaching them without a nil check is plausible.
func pnNilReceiverAll() []map[string]any {
	probe := func(f func()) bool {
		panicked := false
		func() {
			defer func() {
				if r := recover(); r != nil {
					panicked = true
				}
			}()
			f()
		}()
		return panicked
	}

	var audience *model.NoticeAudience
	var clientType *model.NoticeClientType
	var instanceType *model.NoticeInstanceType
	var sku *model.NoticeSKU

	return []map[string]any{
		{"name": "audience", "panics": probe(func() { audience.Matches(true, true) })},
		{"name": "client_type", "panics": probe(func() { clientType.Matches("web") })},
		{"name": "instance_type", "panics": probe(func() { instanceType.Matches(true) })},
		{"name": "sku", "panics": probe(func() { sku.Matches("e10") })},
	}
}

// --- Marshal / Unmarshal -----------------------------------------------------------------------

func pnRoundTripAll() []map[string]any {
	notices := model.ProductNotices{
		{ID: "a", Repeatable: pnBool(true)},
		{ID: "b"},
	}
	blob, err := notices.Marshal()
	if err != nil {
		panic(err)
	}
	decoded, err := model.UnmarshalProductNotices(blob)
	if err != nil {
		panic(err)
	}
	reblob, err := decoded.Marshal()
	if err != nil {
		panic(err)
	}

	messages := model.NoticeMessages{
		{NoticeMessageInternal: model.NoticeMessageInternal{Title: "t", Description: "d"}, ID: "a", SysAdminOnly: true},
	}
	mblob, err := messages.Marshal()
	if err != nil {
		panic(err)
	}
	mdecoded, err := model.UnmarshalProductNoticeMessages(strings.NewReader(string(mblob)))
	if err != nil {
		panic(err)
	}
	mreblob, err := mdecoded.Marshal()
	if err != nil {
		panic(err)
	}

	// An empty document and a null decode both round-trip; the difference is what they produce.
	nullDecoded, nullErr := model.UnmarshalProductNotices([]byte("null"))
	nullBlob, err := nullDecoded.Marshal()
	if err != nil {
		panic(err)
	}

	return []map[string]any{
		{"name": "notices", "marshal": string(blob), "remarshal": string(reblob)},
		{"name": "messages", "marshal": string(mblob), "remarshal": string(mreblob)},
		{
			"name":      "notices_from_null",
			"remarshal": string(nullBlob),
			"ok":        nullErr == nil,
			"is_nil":    nullDecoded == nil,
		},
	}
}
