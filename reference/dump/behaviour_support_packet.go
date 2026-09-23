package main

// Behavioural oracle for the Support Packet's file formats, written to
// fixtures/behaviour_goyaml.json (public inputs only: model types, goccy/go-yaml,
// go-multierror) and fixtures/behaviour_support_packet.json (the platform half, which carries
// transcribed AGPL code and therefore feeds `mm-app`'s tests only — the rule
// behaviour_round_off.go states, [D-031]).
//
// # Why the YAML needs an oracle at all
//
// Five of the packet's files are `github.com/goccy/go-yaml` output, and the encoder has opinions
// no reader would guess: which strings it quotes (a trailing colon, a `- ` anywhere, a value
// `time.Parse` accepts under any of five layouts, every YAML 1.1 boolean), that a multi-line
// string becomes a `|-` block indented from its *column*, that a float with no point gains `.0`,
// that a nil slice is `null` and an empty one `[]`, and where a head comment lands relative to
// a key. The Rust emitter (`mm_model::goyaml`) is checked against these bytes, not against a
// reading of the encoder.
//
// # The comment map is transcribed
//
// `diagnosticsYAMLComments` (app/platform/support_packet.go:40) is unexported, so it is copied
// here character for character. **Copy any upstream change the same way.** The encoder that
// consumes it is goccy's own.
//
// # So is `detectSAMLProviderType`
//
// Same reason, same rule: support_packet.go:600-641, verbatim.
//
// Determinism: fixed inputs throughout; the generator pins TZ, and the one zone-dependent
// formatter (`timeutils.FormatMillis`, which the job, role and scheme marshallers call) records
// the zone it ran in.

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/goccy/go-yaml"
	"github.com/hashicorp/go-multierror"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/utils"
)

func yamlOf(v any) string {
	b, err := yaml.Marshal(v)
	if err != nil {
		return "ERROR: " + err.Error()
	}
	return string(b)
}

func writeGoYAMLBehaviourFixture(outDir string) error {
	zone, offset := time.Now().Zone()
	out := map[string]any{
		"zone":             zone,
		"zone_offset_secs": offset,
		"strings":          goyamlStrings(),
		"floats":           goyamlFloats(),
		"shapes":           goyamlShapes(),
		"stats":            goyamlStats(),
		"jobs":             goyamlJobs(),
		"permissions":      goyamlPermissions(),
		"schema":           goyamlSchema(),
		"metadata":         goyamlMetadata(),
		"multierror":       multierrorCases(),
		"sanitize_data_source": sanitizeDataSourceCases(),
		"sanitize_file_name":   sanitizeFileNameCases(),
		"plugin_settings_sanitize": pluginSettingsSanitizeCases(),
		"plugin_list":      pluginListCases(),
	}
	return writeJSONFixture(outDir, "behaviour_goyaml.json", out)
}

func writeSupportPacketBehaviourFixture(outDir string) error {
	out := map[string]any{
		"diagnostics":        diagnosticsCases(),
		"saml_provider_type": samlProviderCases(),
	}
	return writeJSONFixture(outDir, "behaviour_support_packet.json", out)
}

func writeJSONFixture(outDir, name string, out any) error {
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, name)
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

// goyamlStringInputs are the scalars whose quoting is decided by `token.IsNeedQuoted` and whose
// layout by `StringNode.String`. Every printable ASCII byte is also tried alone and in the first,
// middle and last position of a word, because the rule is a set of byte tests.
func goyamlStringInputs() []string {
	in := []string{
		"", "abc", "a b", " a", "a ", "true", "True", "TRUE", "false", "yes", "Yes", "no", "NO",
		"on", "Off", "y", "Y", "n", "N", "null", "Null", "NULL", "~", ".inf", "-.inf", ".nan",
		".NaN", "NaN", "inf", "0", "123", "-123", "+123", "0123", "0x1F", "0o17", "0b101", "1_000",
		"_1", "1.5", ".5", "1.", "1e5", "1E5", "1.5e3", "1e", "12e3x", "1.2.3", "9223372036854775808",
		"18446744073709551616", "-9223372036854775809", "99999999999999999999999", "1e999",
		"0x", "0b", "0o", "-", "--", "- a", "-a", "a-", "a - b", "a- b", "a -b", ":", "a:", "a: b",
		"a:b", "a :b", "http://x.y/z", "#", "a#b", "a #b", "a\\b", "*x", "&x", "!x", "|x", ">x",
		"%x", "'x", "\"x", "@x", "`x", "[x]", "{x}", "]x", "}x", ",x", "x,", "x]", "x}", "?x",
		"=", "a=b", "2006-01-02", "2006-01-02T15:04:05Z", "2006-01-02t15:04:05Z",
		"2006-01-02T15:04:05.999999999+05:30", "2006-01-02 15:04:05", "15:4", "12:30", "1:2",
		"25:61", "2006-13-45", "multi\nline", "multi\nline\n", "multi\n\n", "\nlead", "a\n b",
		"x\r\ny", "x\ry", "tab\there", "\x01", "\x7f", "héllo", "日本", " ", "a'b", "a\"b",
		"CREATE UNIQUE INDEX idx_posts ON public.posts USING btree (id, createat)",
		"character varying", "en_US.utf8", "UTF8", "C", "json", "jsonb", "timestamp",
		"sanitized", "********************************", "system_admin", "mm_support_packet",
		"a\x00b",
	}
	for c := byte(0x20); c < 0x7f; c++ {
		s := string([]byte{c})
		in = append(in, s, s+"a", "a"+s+"b", "a"+s)
	}
	return in
}

func goyamlStrings() []map[string]any {
	type nested struct {
		Outer struct {
			Inner string   `yaml:"inner"`
			List  []string `yaml:"list"`
		} `yaml:"outer"`
		Seq []struct {
			Name  string `yaml:"name"`
			Value string `yaml:"value"`
		} `yaml:"seq"`
	}
	var cases []map[string]any
	for _, s := range goyamlStringInputs() {
		var n nested
		n.Outer.Inner = s
		n.Outer.List = []string{s, "x"}
		n.Seq = append(n.Seq, struct {
			Name  string `yaml:"name"`
			Value string `yaml:"value"`
		}{Name: "n", Value: s})
		cases = append(cases, map[string]any{
			"input":  s,
			"top":    yamlOf(map[string]string{"k": s}),
			"nested": yamlOf(&n),
			"key":    yamlOf(map[string]int{s: 1}),
		})
	}
	return cases
}

func goyamlFloats() []map[string]any {
	in := []float64{0, 1, -1, 0.5, 0.1 + 0.2, 1.0 / 3.0, 100, 1e20, 1e21, 1e-4, 1e-5, 1e-7,
		123456789, 1234567890123, 0.000123, 2.5e-10, 9.999999e22, 5e-324, 1.7976931348623157e308}
	var cases []map[string]any
	for _, f := range in {
		cases = append(cases, map[string]any{
			"input": f,
			"yaml":  yamlOf(map[string]float64{"k": f}),
		})
	}
	return cases
}

// goyamlShapes: the containers — nil and empty slices and maps, pointers, nested empties, a
// sequence of mappings inside a sequence of mappings.
func goyamlShapes() map[string]string {
	type item struct {
		A string   `yaml:"a"`
		B []string `yaml:"b"`
	}
	type withOmit struct {
		S string            `yaml:"s,omitempty"`
		I int               `yaml:"i,omitempty"`
		L []string          `yaml:"l,omitempty"`
		M map[string]string `yaml:"m,omitempty"`
		P *int              `yaml:"p,omitempty"`
		T time.Time         `yaml:"t,omitempty"`
		Z struct {
			X string `yaml:"x,omitempty"`
		} `yaml:"z,omitempty"`
		Keep string `yaml:"keep"`
	}
	zero := 0
	sp := "s"
	return map[string]string{
		"nil_slice":          yamlOf(struct{ L []string }{}),
		"empty_slice":        yamlOf(struct{ L []string }{L: []string{}}),
		"nil_map":            yamlOf(struct{ M map[string]string }{}),
		"empty_map":          yamlOf(struct{ M map[string]string }{M: map[string]string{}}),
		"sorted_map":         yamlOf(struct{ M map[string]string }{M: map[string]string{"b": "2", "a": "1", "B": "3", "_": "4", "10": "x", "9": "y"}}),
		"nil_ptr":            yamlOf(struct{ P *string }{}),
		"ptr":                yamlOf(struct{ P *string }{P: &sp}),
		"zero_ptr":           yamlOf(struct{ P *int }{P: &zero}),
		"bools":              yamlOf(struct{ T, F bool }{T: true}),
		"ints":               yamlOf(struct{ A, B, C int64 }{A: -1, B: 0, C: 9223372036854775807}),
		"uints":              yamlOf(struct{ U uint64 }{U: 18446744073709551615}),
		"seq_of_maps":        yamlOf(struct{ S []item }{S: []item{{A: "1", B: []string{"x", "y"}}, {A: "2", B: []string{}}, {A: "3"}}}),
		"seq_of_seqs":        yamlOf(struct{ S [][]string }{S: [][]string{{"a", "b"}, {}, nil, {"c"}}}),
		"nested_empty":       yamlOf(struct{ O struct{ I struct{} } }{}),
		"map_of_maps":        yamlOf(map[string]map[string]string{"x": {"b": "1", "a": "2"}, "y": {}, "z": nil}),
		"omitempty_zero":     yamlOf(withOmit{}),
		"omitempty_set":      yamlOf(withOmit{S: "s", I: 1, L: []string{"a"}, M: map[string]string{"k": "v"}, P: &zero, T: time.Date(2024, 1, 2, 3, 4, 5, 600000000, time.UTC), Keep: "k"}),
		"omitempty_empties":  yamlOf(withOmit{L: []string{}, M: map[string]string{}}),
		"multiline_in_seq":   yamlOf(struct{ S []string }{S: []string{"a\nb", "c\nd\n"}}),
		"multiline_deep":     yamlOf(struct{ A struct{ B struct{ C []item } } }{A: struct{ B struct{ C []item } }{B: struct{ C []item }{C: []item{{A: "x\ny", B: []string{"p\nq"}}}}}}),
		"time_utc":           yamlOf(struct{ T time.Time }{T: time.Date(2024, 1, 2, 3, 4, 5, 0, time.UTC)}),
		"time_nanos":         yamlOf(struct{ T time.Time }{T: time.Date(2024, 1, 2, 3, 4, 5, 120000000, time.UTC)}),
		"time_zero":          yamlOf(struct{ T time.Time }{}),
		"time_ptr_nil":       yamlOf(struct{ T *time.Time }{}),
		"string_map_any":     yamlOf(map[string]any{}),
		"top_empty_struct":   yamlOf(struct{}{}),
	}
}

func goyamlStats() map[string]any {
	stats := model.SupportPacketStats{
		RegisteredUsers: 101, ActiveUsers: 102, DailyActiveUsers: 103, MonthlyActiveUsers: 104,
		DeactivatedUsers: 105, Guests: 106, SingleChannelGuests: 107, BotAccounts: 108,
		Posts: 109, Channels: 110, Teams: 111, SlashCommands: 112, IncomingWebhooks: 113,
		OutgoingWebhooks: 114,
	}
	return map[string]any{
		"populated": yamlOf(&stats),
		"zero":      yamlOf(&model.SupportPacketStats{}),
	}
}

func goyamlJobs() map[string]any {
	jobs := []*model.Job{
		{
			Id: "jobid1111111111111111111a", Type: model.JobTypeLdapSync, Priority: 3,
			CreateAt: 1700000000123, StartAt: 1700000001000, LastActivityAt: 0,
			Status: model.JobStatusSuccess, Progress: 100,
			Data: model.StringMap{"b_key": "value: with colon", "a_key": "plain", "empty": "",
				"multi": "line one\nline two", "num": "42"},
		},
		{
			Id: "jobid2222222222222222222b", Type: model.JobTypeMigrations, Priority: 0,
			CreateAt: 1, StartAt: -1, LastActivityAt: 1699999999999, Status: model.JobStatusError,
			Progress: -1, Data: model.StringMap{},
		},
		{Id: "jobid3333333333333333333c", Type: model.JobTypeMessageExport, Data: nil},
	}
	list := model.SupportPacketJobList{
		LDAPSyncJobs:               jobs[:1],
		DataRetentionJobs:          []*model.Job{},
		MessageExportJobs:          jobs[2:],
		ElasticPostIndexingJobs:    nil,
		ElasticPostAggregationJobs: jobs,
		MigrationJobs:              jobs[1:2],
	}
	input, _ := json.Marshal(jobs)
	return map[string]any{
		"input_jobs": json.RawMessage(input),
		"yaml":       yamlOf(&list),
		"empty":      yamlOf(&model.SupportPacketJobList{DataRetentionJobs: []*model.Job{}}),
	}
}

func goyamlPermissions() map[string]any {
	schemeID := "schemeid11111111111111111a"
	empty := ""
	roles := []*model.Role{
		{Id: "roleid111111111111111111aa", Name: "system_admin", DisplayName: "authentication.roles.global_admin.name",
			Description: "desc: with colon", CreateAt: 1700000000000, UpdateAt: 1700000000500, DeleteAt: 0,
			Permissions: []string{"manage_system", "create_team"}, SchemeManaged: true, BuiltIn: true},
		{Id: "roleid222222222222222222bb", Name: "team_user", Permissions: []string{}, SchemeId: &schemeID},
		{Id: "roleid333333333333333333cc", Name: "x", Permissions: nil, SchemeId: &empty},
	}
	schemes := []*model.Scheme{
		{Id: "schemeid11111111111111111a", Name: "sch", DisplayName: "Scheme", Description: "multi\nline",
			CreateAt: 1700000000000, UpdateAt: 1700000000001, DeleteAt: 1700000000002, Scope: "team",
			DefaultTeamAdminRole: "ta", DefaultTeamUserRole: "tu", DefaultChannelAdminRole: "ca",
			DefaultChannelUserRole: "cu", DefaultTeamGuestRole: "tg", DefaultChannelGuestRole: "cg",
			DefaultPlaybookAdminRole: "pa", DefaultPlaybookMemberRole: "pm", DefaultRunAdminRole: "ra",
			DefaultRunMemberRole: "rm"},
	}
	rolesJSON, _ := json.Marshal(roles)
	schemesJSON, _ := json.Marshal(schemes)
	for _, r := range roles {
		r.Sanitize()
	}
	for _, s := range schemes {
		s.Sanitize()
	}
	return map[string]any{
		"input_roles":   json.RawMessage(rolesJSON),
		"input_schemes": json.RawMessage(schemesJSON),
		"yaml":          yamlOf(&model.SupportPacketPermissionInfo{Roles: roles, Schemes: schemes}),
		"no_schemes":    yamlOf(&model.SupportPacketPermissionInfo{Roles: roles[:1]}),
	}
}

func goyamlSchema() map[string]any {
	schema := model.SupportPacketDatabaseSchema{
		DatabaseCollation: "en_US.utf8",
		DatabaseEncoding:  "UTF8",
		Tables: []model.DatabaseTable{
			{
				Name: "posts", Collation: "default",
				Options: map[string]string{"fillfactor": "90", "autovacuum_enabled": "true"},
				Columns: []model.DatabaseColumn{
					{Name: "id", DataType: "character varying", MaxLength: 26, IsNullable: false},
					{Name: "props", DataType: "jsonb", MaxLength: 0, IsNullable: true},
				},
				Indexes: []model.DatabaseIndex{
					{Name: "posts_pkey", Definition: "CREATE UNIQUE INDEX posts_pkey ON public.posts USING btree (id)"},
				},
			},
			{Name: "empty", Columns: []model.DatabaseColumn{}},
		},
	}
	return map[string]any{
		"yaml":  yamlOf(&schema),
		"empty": yamlOf(&model.SupportPacketDatabaseSchema{}),
	}
}

func goyamlMetadata() map[string]any {
	md := &model.PacketMetadata{
		Version: 1, Type: model.SupportPacketType, GeneratedAt: 1700000000123,
		ServerVersion: "11.1.0", ServerID: "serverid111111111111111111", LicenseID: "licenseid11111111111111111",
		CustomerID: "customerid1111111111111111", Extras: map[string]any{},
	}
	unlicensed := *md
	unlicensed.LicenseID = ""
	unlicensed.CustomerID = ""
	return map[string]any{
		"licensed":   yamlOf(md),
		"unlicensed": yamlOf(&unlicensed),
	}
}

// multierrorCases records go-multierror's `Error()` text for the shapes the packet builds,
// including the two that matter: `Append(err)` with a plain error as the *first* argument (which
// starts a new list, discarding nothing but also appending to nothing), and `Append(list, list)`
// (which flattens).
func multierrorCases() map[string]string {
	e1, e2, e3 := errors.New("first"), errors.New("second"), errors.New("third\nwith newline")
	var one *multierror.Error
	one = multierror.Append(one, e1)
	var two *multierror.Error
	two = multierror.Append(two, e1, e2)
	var nested *multierror.Error
	nested = multierror.Append(nested, e3)
	nested = multierror.Append(nested, two)
	restart := multierror.Append(e1)
	restart = multierror.Append(e2)
	return map[string]string{
		"one":     one.Error(),
		"two":     two.Error(),
		"nested":  nested.Error(),
		"restart": restart.Error(),
	}
}

func sanitizeDataSourceCases() []map[string]any {
	in := []string{
		"",
		"postgres://mmuser:mostest@localhost/mattermost_test?sslmode=disable&connect_timeout=10",
		"postgres://mmuser:mostest@localhost:5432/mattermost_test",
		"postgres://localhost/db?user=u&password=p&sslmode=require",
		"postgres://u@host/db",
		"postgres://u:p%40ss@host/db?application_name=a%20b",
		"postgres://host/db?b=2&a=1&a=0",
		"host=localhost user=mmuser password=secret dbname=mattermost",
		"::not a url",
		"postgres://u:p@[::1]:5432/db?search_path=x,y",
	}
	var cases []map[string]any
	for _, dsn := range in {
		for _, driver := range []string{model.DatabaseDriverPostgres, "mysql"} {
			out, err := model.SanitizeDataSource(driver, dsn)
			c := map[string]any{"driver": driver, "input": dsn, "output": out}
			if err != nil {
				c["error"] = err.Error()
			}
			cases = append(cases, c)
		}
	}
	return cases
}

func sanitizeFileNameCases() []map[string]string {
	in := []string{"", "Acme Corp.", "  .Acme. ", "a.b.c", "Ünïcødé Co", "x/y\\z", "tab\tco",
		"dash-and_underscore", "日本", strings.Repeat("a", 120), strings.Repeat("é", 60), "..", ". .",
		"Mattermost Inc."}
	var cases []map[string]string
	for _, s := range in {
		cases = append(cases, map[string]string{"input": s, "output": utils.SanitizeFileName(s)})
	}
	return cases
}

func pluginSettingsSanitizeCases() map[string]any {
	manifests := []*model.Manifest{
		{Id: "withschema", SettingsSchema: &model.PluginSettingsSchema{
			Settings: []*model.PluginSetting{{Key: "Secret", Secret: true}, {Key: "Plain"}},
			Sections: []*model.PluginSettingsSection{{Key: "s", Settings: []*model.PluginSetting{{Key: "SectionSecret", Secret: true}}}},
		}},
		{Id: "noschema"},
	}
	plugins := func() map[string]map[string]any {
		return map[string]map[string]any{
			"withschema":  {"secret": "x", "plain": "y", "sectionsecret": "z", "other": 1},
			"noschema":    {"secret": "x"},
			"notinstalled": {"a": "b"},
			"emptysettings": {},
			// What `SavePluginConfig(nil)` leaves behind: kept, as the empty map is.
			"nilsettings": nil,
		}
	}
	withManifests := model.PluginSettings{Plugins: plugins()}
	withManifests.Sanitize(manifests)
	withNil := model.PluginSettings{Plugins: plugins()}
	withNil.Sanitize(nil)
	manifestsJSON, _ := json.Marshal(manifests)
	return map[string]any{
		"input":          plugins(),
		"manifests":      json.RawMessage(manifestsJSON),
		"with_manifests": withManifests.Plugins,
		"with_nil":       withNil.Plugins,
	}
}

// pluginListCases: `SupportPacketPluginList` through `json.MarshalIndent`, as getPluginsFile
// writes it — with no plugins the two slices are nil, and nil marshals as `null`.
func pluginListCases() map[string]string {
	marshal := func(v any) string {
		b, err := json.MarshalIndent(v, "", "    ")
		if err != nil {
			return "ERROR: " + err.Error()
		}
		return string(b)
	}
	return map[string]string{
		"empty": marshal(model.SupportPacketPluginList{}),
		"one": marshal(model.SupportPacketPluginList{
			Enabled: []model.Manifest{{Id: "a<b>&c", Name: "N", Version: "1.0.0"}},
		}),
	}
}

// ---------------------------------------------------------------------------------------------
// The platform half (AGPL)
// ---------------------------------------------------------------------------------------------

// diagnosticsYAMLComments is app/platform/support_packet.go:40-80, transcribed.
var diagnosticsYAMLComments = yaml.CommentMap{
	// server: — grouped into Machine / Capacity / Process lifecycle / Software
	"$.server.os":                        {yaml.HeadComment(" Machine")},
	"$.server.cpu_cores":                 {yaml.HeadComment(" Capacity (hardware → effective quota)"), yaml.LineComment(" logical CPUs visible to the OS")},
	"$.server.total_memory_mb":           {yaml.LineComment(" host/VM total RAM; may exceed container limit")},
	"$.server.container_cpu_limit":       {yaml.LineComment(" cgroup v2 CPU quota in CPUs; Linux only, omitted if no limit set")},
	"$.server.container_memory_limit_mb": {yaml.LineComment(" cgroup v2 memory quota in MB; Linux only, omitted if no limit set")},
	"$.server.process_id":                {yaml.HeadComment(" Process lifecycle")},
	"$.server.started_at":                {yaml.LineComment(" when Mattermost process started")},
	"$.server.host_started_at":           {yaml.LineComment(" when the host OS booted; omitted if unavailable")},
	"$.server.open_file_descriptors":     {yaml.LineComment(" current open FDs for this process")},
	"$.server.max_file_descriptors":      {yaml.LineComment(" system limit (ulimit -n)")},
	"$.server.version":                   {yaml.HeadComment(" Software")},

	// database: sql.DBStats cumulative counters (lifetime of process; all drivers)
	"$.database.master_pool_wait_count":                  {yaml.LineComment(" cumulative; total times a goroutine waited for a connection since process start")},
	"$.database.master_pool_wait_duration_ms":            {yaml.LineComment(" cumulative wait time across all goroutines since process start")},
	"$.database.master_connections_closed_max_idle":      {yaml.LineComment(" cumulative; connections closed because the idle pool was full")},
	"$.database.master_connections_closed_max_lifetime":  {yaml.LineComment(" cumulative; connections closed for exceeding ConnMaxLifetime")},
	"$.database.replica_pool_wait_count":                 {yaml.LineComment(" cumulative across all replicas; see master_pool_wait_count")},
	"$.database.replica_pool_wait_duration_ms":           {yaml.LineComment(" cumulative across all replicas")},
	"$.database.replica_connections_closed_max_idle":     {yaml.LineComment(" cumulative across all replicas")},
	"$.database.replica_connections_closed_max_lifetime": {yaml.LineComment(" cumulative across all replicas")},

	// database: PostgreSQL-only fields (omitted on MySQL)
	"$.database.cache_hit_ratio":                {yaml.HeadComment(" PostgreSQL-only (these fields are omitted on MySQL)"), yaml.LineComment(" blks_hit / (blks_hit + blks_read) from pg_stat_database; cumulative since stats reset")},
	"$.database.deadlocks":                      {yaml.LineComment(" cumulative since pg_stat_database reset")},
	"$.database.temp_files":                     {yaml.LineComment(" cumulative count of temp files created since stats reset")},
	"$.database.temp_bytes_mb":                  {yaml.LineComment(" cumulative bytes written to temp files, in MB")},
	"$.database.rollbacks":                      {yaml.LineComment(" cumulative transaction rollbacks since stats reset")},
	"$.database.idle_in_transaction_count":      {yaml.LineComment(" point-in-time count from pg_stat_activity")},
	"$.database.longest_query_duration_seconds": {yaml.LineComment(" point-in-time; max age of any active query right now")},
	"$.database.waiting_for_lock_count":         {yaml.LineComment(" point-in-time count of backends waiting on a Lock wait_event_type")},
	"$.database.posts_dead_tuples":              {yaml.LineComment(" n_dead_tup for the posts table from pg_stat_user_tables")},
	"$.database.posts_last_autovacuum":          {yaml.LineComment(" last autovacuum on posts; null if never autovacuumed (then omitted)")},

	// file_store: local driver only fields
	"$.file_store.filesystem_type": {yaml.LineComment(" local driver only (e.g. ext4, xfs); omitted for s3 and other remote drivers")},
	"$.file_store.total_mb":        {yaml.LineComment(" local driver only; capacity of the volume hosting FileSettings.Directory")},
	"$.file_store.available_mb":    {yaml.LineComment(" local driver only; free space remaining on that volume")},
}

func diagnosticsYAML(d *model.SupportPacketDiagnostics) string {
	b, err := yaml.MarshalWithOptions(d, yaml.WithComment(diagnosticsYAMLComments))
	if err != nil {
		return "ERROR: " + err.Error()
	}
	return string(b)
}

func diagnosticsCases() []map[string]any {
	f := func(v float64) *float64 { return &v }
	i := func(v int64) *int64 { return &v }
	autovac := time.Date(2025, 3, 4, 5, 6, 7, 890000000, time.UTC)

	full := &model.SupportPacketDiagnostics{Version: model.CurrentSupportPacketVersion}
	full.License.Company = "Acme: Inc"
	full.License.Users = 1000
	full.License.SkuShortName = "enterprise"
	full.License.IsTrial = true
	full.License.IsGovSKU = true
	full.License.IsNonProduction = true
	full.Server.OS = "linux"
	full.Server.Architecture = "amd64"
	full.Server.Hostname = "host-1"
	full.Server.InstallationType = "unknown"
	full.Server.CPUCores = 16
	full.Server.TotalMemoryMB = 64000
	full.Server.ContainerCPULimit = 1.5
	full.Server.ContainerMemoryLimitMB = 2048
	full.Server.ProcessID = 4242
	full.Server.StartedAt = time.Date(2025, 1, 2, 3, 4, 5, 123456789, time.UTC)
	full.Server.HostStartedAt = time.Date(2025, 1, 1, 0, 0, 0, 0, time.UTC)
	full.Server.OpenFileDescriptors = 77
	full.Server.MaxFileDescriptors = 1048576
	full.Server.Version = "11.1.0"
	full.Server.BuildHash = "abc123"
	full.Server.GoVersion = "go1.26.4"
	full.Config.Source = "postgres://****:****@localhost/mattermost_test?sslmode=disable"
	full.Database.Type = "postgres"
	full.Database.Version = "16.4 (Debian 16.4-1.pgdg120+1)"
	full.Database.SchemaVersion = "150"
	full.Database.MasterConnections = 11
	full.Database.ReplicaConnections = 12
	full.Database.SearchConnections = 13
	full.Database.MasterConnectionsInUse = 14
	full.Database.MasterConnectionsIdle = 15
	full.Database.MasterPoolWaitCount = 16
	full.Database.MasterPoolWaitDurationMs = 17
	full.Database.MasterConnectionsClosedMaxIdle = 18
	full.Database.MasterConnectionsClosedMaxLifetime = 19
	full.Database.ReplicaConnectionsInUse = 20
	full.Database.ReplicaConnectionsIdle = 21
	full.Database.ReplicaPoolWaitCount = 22
	full.Database.ReplicaPoolWaitDurationMs = 23
	full.Database.ReplicaConnectionsClosedMaxIdle = 24
	full.Database.ReplicaConnectionsClosedMaxLifetime = 25
	full.Database.CacheHitRatio = f(0.9876)
	full.Database.Deadlocks = i(26)
	full.Database.TempFiles = i(27)
	full.Database.TempBytesMB = f(1.25)
	full.Database.Rollbacks = i(28)
	full.Database.IdleInTransactionCount = i(29)
	full.Database.LongestQueryDurationSeconds = f(0.004211)
	full.Database.WaitingForLockCount = i(30)
	full.Database.PostsDeadTuples = i(31)
	full.Database.PostsLastAutovacuum = &autovac
	full.FileStore.Status = "FAIL"
	full.FileStore.Error = "open /nope: no such file"
	full.FileStore.Driver = "local"
	full.FileStore.FilesystemType = "ext4"
	full.FileStore.TotalMB = 100000
	full.FileStore.AvailableMB = 50000
	full.Websocket.Connections = 3
	full.Cluster.ID = "clusterid"
	full.Cluster.NumberOfNodes = 2
	full.Notifications.Email.Status = "FAIL"
	full.Notifications.Email.Error = "dial tcp: connection refused"
	full.Notifications.Push.Status = "OK"
	full.LDAP.Status = "OK"
	full.LDAP.ServerName = "OpenLDAP"
	full.LDAP.ServerVersion = "2.6"
	full.SAML.ProviderType = "Okta"
	full.SAML.Status = "FAIL"
	full.SAML.Error = "bad cert"
	full.ElasticSearch.Status = "OK"
	full.ElasticSearch.Backend = "elasticsearch"
	full.ElasticSearch.ServerVersion = "8.9.0"
	full.ElasticSearch.ServerPlugins = []string{"analysis-icu", "ingest"}
	full.ElasticSearch.Error = "e"
	full.OAuthProviders.GitLab = model.OAuthProviderStatus{Status: "disabled"}
	full.OAuthProviders.Google = model.OAuthProviderStatus{Status: "FAIL", Error: "no discovery or token endpoint configured"}
	full.OAuthProviders.Office365 = model.OAuthProviderStatus{Status: "OK"}
	full.OAuthProviders.OpenID = model.OAuthProviderStatus{Status: "FAIL", Error: "multi\nline"}

	// What a Team Edition stack actually produces: no licence, no cluster, no LDAP/SAML/ES, and
	// every `omitempty` field at its zero.
	stack := &model.SupportPacketDiagnostics{Version: model.CurrentSupportPacketVersion}
	stack.Server.OS = "linux"
	stack.Server.StartedAt = time.Date(2025, 1, 2, 3, 4, 5, 0, time.UTC)
	stack.Database.CacheHitRatio = f(0)
	stack.Database.Deadlocks = i(0)
	stack.FileStore.Status = "OK"
	stack.FileStore.Driver = "local"
	stack.Notifications.Email.Status = "disabled"
	stack.Notifications.Push.Status = "disabled"
	stack.LDAP.Status = "disabled"
	stack.SAML.Status = "disabled"
	stack.ElasticSearch.Status = "disabled"
	stack.OAuthProviders.GitLab.Status = "disabled"
	stack.OAuthProviders.Google.Status = "disabled"
	stack.OAuthProviders.Office365.Status = "disabled"
	stack.OAuthProviders.OpenID.Status = "disabled"

	zero := &model.SupportPacketDiagnostics{}

	var cases []map[string]any
	for _, c := range []struct {
		name string
		d    *model.SupportPacketDiagnostics
	}{{"full", full}, {"stack", stack}, {"zero", zero}} {
		input, _ := json.Marshal(c.d)
		cases = append(cases, map[string]any{
			"name":  c.name,
			"input": json.RawMessage(input),
			"yaml":  diagnosticsYAML(c.d),
		})
	}
	return cases
}

// detectSAMLProviderType is app/platform/support_packet.go:600-641, transcribed.
func detectSAMLProviderType(idpDescriptorURL string) string {
	if idpDescriptorURL == "" {
		return "unknown"
	}

	normalizedURL := strings.ToLower(idpDescriptorURL)

	switch {
	case strings.Contains(normalizedURL, "login.microsoftonline.com") || strings.Contains(normalizedURL, "sts.windows.net"):
		return "Azure AD"
	case strings.Contains(normalizedURL, ".okta.com") || strings.Contains(normalizedURL, ".oktapreview.com"):
		return "Okta"
	case strings.Contains(normalizedURL, ".auth0.com"):
		return "Auth0"
	case strings.Contains(normalizedURL, ".onelogin.com"):
		return "OneLogin"
	case strings.Contains(normalizedURL, "accounts.google.com"):
		return "Google Workspace"
	case strings.Contains(normalizedURL, "sso.jumpcloud.com"):
		return "JumpCloud"
	case strings.Contains(normalizedURL, "duo.com/saml2"):
		return "Duo"
	case strings.Contains(normalizedURL, ".centrify.com"):
		return "Centrify"
	case strings.Contains(normalizedURL, "/realms/"):
		return "Keycloak"
	case strings.Contains(normalizedURL, "/adfs") || strings.Contains(normalizedURL, "/federationmetadata/"):
		return "ADFS"
	case strings.Contains(normalizedURL, "shibboleth.net") || strings.Contains(normalizedURL, "/idp/shibboleth"):
		return "Shibboleth"
	default:
		return "unknown"
	}
}

func samlProviderCases() []map[string]string {
	in := []string{"", "https://login.microsoftonline.com/x/federationmetadata/2007-06/federationmetadata.xml",
		"https://STS.WINDOWS.NET/tenant/", "https://dev-1.okta.com/app/x/sso/saml/metadata",
		"https://x.oktapreview.com", "https://tenant.auth0.com/samlp/metadata", "https://x.onelogin.com/saml",
		"https://accounts.google.com/o/saml2?idpid=1", "https://sso.jumpcloud.com/saml2/x", "https://api.duo.com/saml2/sp",
		"https://x.centrify.com/", "https://kc.example.com/realms/master/protocol/saml/descriptor",
		"https://fs.example.com/adfs/ls", "https://x.example.com/FederationMetadata/2007-06/FederationMetadata.xml",
		"https://idp.shibboleth.net/", "https://example.edu/idp/shibboleth", "https://example.com/saml",
		"https://login.microsoftonline.com.okta.com/", "https://x.okta.com/realms/y"}
	var cases []map[string]string
	for _, s := range in {
		cases = append(cases, map[string]string{"input": s, "output": detectSAMLProviderType(s)})
	}
	return cases
}
