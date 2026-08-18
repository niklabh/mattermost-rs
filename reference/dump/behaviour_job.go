package main

// Behavioural oracle for model.Job and utils/timeutils, written to fixtures/behaviour_job.json.
//
// # `AllJobTypes` is not all the job types
//
// job.go declares **43** `JobType*` constants and `AllJobTypes` lists **24** of them. Since
// `IsValidJobType` is a linear scan of that array, nineteen declared job types are **rejected by
// the model's own validator** — including `cli_message_export`, `recap`, `push_proxy_auth` and
// every `*_notify_admin`. Whether that is a bug or a deliberate "these are the schedulable ones"
// list is not something a reading can settle, so the corpus asks Go about every constant by name
// and records the answer. A transcribed list would have been 24 lines of hope.
//
// # `IsValid` never checks the type
//
// `IsValidJobType` exists and `Job.IsValid` does not call it. So a job with a nonsense type is
// valid as far as the model is concerned; only the scheduler narrows it.
//
// # `IsValidStatusChange` is a partial transition table
//
// Three source states have transitions and four do not, so `success`, `error`, `warning` and
// `canceled` are terminal — and so is any status not in the list at all. Driven as a full matrix
// rather than as the three cases the switch names, because "everything else is false" is the part
// that is easy to get wrong.
//
// # `FormatMillis` is server-timezone dependent, and elides trailing zeros
//
//	const RFC3339Milli = "2006-01-02T15:04:05.999Z07:00"
//	time.UnixMilli(millis).Format(RFC3339Milli)
//
// `time.UnixMilli` attaches `time.Local` ([D-008] all over again), so the offset in the output is
// the **server's**. And Go's `.999` drops trailing zeros *and the decimal point* when the fraction
// is zero — so a whole-second timestamp formats with no fractional part at all. Both are measured
// rather than assumed, and the fixture records the zone it ran under so the Rust test can rebuild
// the instant in that zone.
//
// Determinism: fixed inputs. The generator pins TZ (see main.go), so the recorded offsets are
// stable; the fixture carries the zone name and offset explicitly anyway.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/utils/timeutils"
)

func writeJobBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":              jobConstants(),
		"all_job_types":          model.AllJobTypes[:],
		"is_valid_job_type":      jobIsValidTypeAll(),
		"is_valid_job_status":    jobIsValidStatusAll(),
		"is_valid":               jobIsValidAll(),
		"is_valid_status_change": jobStatusChangeMatrix(),
		"auditable":              jobAuditableProbe(),
		"format_millis":          jobFormatMillisAll(),
		"parse_formated_millis":  jobParseMillisAll(),
		"wire":                   jobWireProbes(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_job.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

// jobTypeConstants pairs every declared JobType constant with its Go identifier, so the Rust
// transcription is checked name by name rather than as an unordered set.
func jobTypeConstants() [][2]string {
	return [][2]string{
		{"JobTypeDataRetention", model.JobTypeDataRetention},
		{"JobTypeMessageExport", model.JobTypeMessageExport},
		{"JobTypeCLIMessageExport", model.JobTypeCLIMessageExport},
		{"JobTypeElasticsearchPostIndexing", model.JobTypeElasticsearchPostIndexing},
		{"JobTypeElasticsearchPostAggregation", model.JobTypeElasticsearchPostAggregation},
		{"JobTypeLdapSync", model.JobTypeLdapSync},
		{"JobTypeMigrations", model.JobTypeMigrations},
		{"JobTypePlugins", model.JobTypePlugins},
		{"JobTypeExpiryNotify", model.JobTypeExpiryNotify},
		{"JobTypeProductNotices", model.JobTypeProductNotices},
		{"JobTypeActiveUsers", model.JobTypeActiveUsers},
		{"JobTypeImportProcess", model.JobTypeImportProcess},
		{"JobTypeImportDelete", model.JobTypeImportDelete},
		{"JobTypeExportProcess", model.JobTypeExportProcess},
		{"JobTypeExportDelete", model.JobTypeExportDelete},
		{"JobTypeCloud", model.JobTypeCloud},
		{"JobTypeResendInvitationEmail", model.JobTypeResendInvitationEmail},
		{"JobTypeExtractContent", model.JobTypeExtractContent},
		{"JobTypeLastAccessiblePost", model.JobTypeLastAccessiblePost},
		{"JobTypeLastAccessibleFile", model.JobTypeLastAccessibleFile},
		{"JobTypeUpgradeNotifyAdmin", model.JobTypeUpgradeNotifyAdmin},
		{"JobTypeTrialNotifyAdmin", model.JobTypeTrialNotifyAdmin},
		{"JobTypePostPersistentNotifications", model.JobTypePostPersistentNotifications},
		{"JobTypeInstallPluginNotifyAdmin", model.JobTypeInstallPluginNotifyAdmin},
		{"JobTypeHostedPurchaseScreening", model.JobTypeHostedPurchaseScreening},
		{"JobTypeS3PathMigration", model.JobTypeS3PathMigration},
		{"JobTypeCleanupDesktopTokens", model.JobTypeCleanupDesktopTokens},
		{"JobTypeDeleteEmptyDraftsMigration", model.JobTypeDeleteEmptyDraftsMigration},
		{"JobTypeRefreshMaterializedViews", model.JobTypeRefreshMaterializedViews},
		{"JobTypeDeleteOrphanDraftsMigration", model.JobTypeDeleteOrphanDraftsMigration},
		{"JobTypeExportUsersToCSV", model.JobTypeExportUsersToCSV},
		{"JobTypeDeleteDmsPreferencesMigration", model.JobTypeDeleteDmsPreferencesMigration},
		{"JobTypeMobileSessionMetadata", model.JobTypeMobileSessionMetadata},
		{"JobTypeAccessControlSync", model.JobTypeAccessControlSync},
		{"JobTypeAccessControlTeamSync", model.JobTypeAccessControlTeamSync},
		{"JobTypePushProxyAuth", model.JobTypePushProxyAuth},
		{"JobTypeRecap", model.JobTypeRecap},
		{"JobTypeScheduledRecap", model.JobTypeScheduledRecap},
		{"JobTypeDeleteExpiredPosts", model.JobTypeDeleteExpiredPosts},
		{"JobTypeAutoTranslationRecovery", model.JobTypeAutoTranslationRecovery},
		{"JobTypeCleanupExpiredAccessTokens", model.JobTypeCleanupExpiredAccessTokens},
		{"JobTypeNotifyExpiringAccessTokens", model.JobTypeNotifyExpiringAccessTokens},
	}
}

func jobStatusConstants() [][2]string {
	return [][2]string{
		{"JobStatusPending", model.JobStatusPending},
		{"JobStatusInProgress", model.JobStatusInProgress},
		{"JobStatusSuccess", model.JobStatusSuccess},
		{"JobStatusError", model.JobStatusError},
		{"JobStatusCancelRequested", model.JobStatusCancelRequested},
		{"JobStatusCanceled", model.JobStatusCanceled},
		{"JobStatusWarning", model.JobStatusWarning},
	}
}

func jobConstants() map[string]any {
	types := map[string]string{}
	for _, pair := range jobTypeConstants() {
		types[pair[0]] = pair[1]
	}
	statuses := map[string]string{}
	for _, pair := range jobStatusConstants() {
		statuses[pair[0]] = pair[1]
	}
	return map[string]any{
		"job_types":            types,
		"job_statuses":         statuses,
		"job_type_count":       len(types),
		"all_job_types_length": len(model.AllJobTypes),
		"rfc3339_milli_layout": timeutils.RFC3339Milli,
	}
}

// jobIsValidTypeAll asks Go about every declared constant plus a few non-constants.
//
// The gap between "declared" and "accepted" is the point: `AllJobTypes` omits nineteen of them.
func jobIsValidTypeAll() []map[string]any {
	var out []map[string]any
	for _, pair := range jobTypeConstants() {
		out = append(out, map[string]any{
			"go_name":           pair[0],
			"value":             pair[1],
			"valid":             model.IsValidJobType(pair[1]),
			"declared_constant": true,
		})
	}
	for _, other := range []string{"", "DATA_RETENTION", "data retention", "unknown_job", "recap "} {
		out = append(out, map[string]any{
			"go_name":           "",
			"value":             other,
			"valid":             model.IsValidJobType(other),
			"declared_constant": false,
		})
	}
	return out
}

func jobIsValidStatusAll() []map[string]any {
	var out []map[string]any
	for _, pair := range jobStatusConstants() {
		out = append(out, map[string]any{
			"go_name": pair[0],
			"value":   pair[1],
			"valid":   model.IsValidJobStatus(pair[1]),
		})
	}
	for _, other := range []string{"", "PENDING", "in progress", "done", "cancelled"} {
		out = append(out, map[string]any{
			"go_name": "",
			"value":   other,
			"valid":   model.IsValidJobStatus(other),
		})
	}
	return out
}

const jobID = "abcdefghijklmnopqrstuvwxyz"

func jobValid() model.Job {
	return model.Job{
		Id:             jobID,
		Type:           model.JobTypeDataRetention,
		Priority:       10,
		CreateAt:       1700000000000,
		StartAt:        1700000001000,
		LastActivityAt: 1700000002000,
		Status:         model.JobStatusPending,
		Progress:       42,
		Data:           model.StringMap{"key": "value"},
	}
}

func jobIsValidAll() []map[string]any {
	corpus := []struct {
		name string
		mut  func(*model.Job)
	}{
		{"valid", func(*model.Job) {}},
		{"bad_id", func(j *model.Job) { j.Id = "nope" }},
		{"empty_id", func(j *model.Job) { j.Id = "" }},
		{"zero_create_at", func(j *model.Job) { j.CreateAt = 0 }},
		{"negative_create_at", func(j *model.Job) { j.CreateAt = -1 }},
		{"empty_status", func(j *model.Job) { j.Status = "" }},
		{"unknown_status", func(j *model.Job) { j.Status = "done" }},
		{"uppercase_status", func(j *model.Job) { j.Status = "PENDING" }},
		// IsValid never calls IsValidJobType, so a nonsense type validates.
		{"empty_type", func(j *model.Job) { j.Type = "" }},
		{"unknown_type", func(j *model.Job) { j.Type = "not_a_job_type" }},
		{"declared_but_unlisted_type", func(j *model.Job) { j.Type = model.JobTypeRecap }},
		// Nothing else is checked either.
		{"negative_priority", func(j *model.Job) { j.Priority = -1 }},
		{"negative_progress", func(j *model.Job) { j.Progress = -1 }},
		{"progress_over_100", func(j *model.Job) { j.Progress = 1000 }},
		{"nil_data", func(j *model.Job) { j.Data = nil }},
		{"zero_start_at", func(j *model.Job) { j.StartAt = 0 }},
		// Ordering: id is checked before create_at, create_at before status.
		{"bad_id_and_zero_create_at", func(j *model.Job) {
			j.Id = "nope"
			j.CreateAt = 0
		}},
		{"zero_create_at_and_bad_status", func(j *model.Job) {
			j.CreateAt = 0
			j.Status = "done"
		}},
	}

	var out []map[string]any
	for _, c := range corpus {
		j := jobValid()
		c.mut(&j)
		err := j.IsValid()
		entry := map[string]any{"name": c.name, "ok": err == nil}
		if err != nil {
			entry["id"] = err.Id
			entry["where"] = err.Where
			entry["status"] = err.StatusCode
			entry["detailed_error"] = err.DetailedError
		}
		out = append(out, entry)
	}
	return out
}

// jobStatusChangeMatrix drives every (current, new) pair, including statuses the switch never
// names — because "everything else returns false" is the half a port drops.
func jobStatusChangeMatrix() []map[string]any {
	states := []string{
		model.JobStatusPending,
		model.JobStatusInProgress,
		model.JobStatusSuccess,
		model.JobStatusError,
		model.JobStatusCancelRequested,
		model.JobStatusCanceled,
		model.JobStatusWarning,
		"", "unknown",
	}

	var out []map[string]any
	for _, current := range states {
		for _, next := range states {
			j := jobValid()
			j.Status = current
			out = append(out, map[string]any{
				"current": current,
				"new":     next,
				"allowed": j.IsValidStatusChange(next),
			})
		}
	}
	return out
}

func jobAuditableProbe() map[string]any {
	j := jobValid()
	a := j.Auditable()
	blob, _ := json.Marshal(a)

	nilData := jobValid()
	nilData.Data = nil
	nilBlob, _ := json.Marshal(nilData.Auditable())

	logClone, _ := json.Marshal(j.LogClone())

	return map[string]any{
		"key_count":                  len(a),
		"json":                       string(blob),
		"nil_data_json":              string(nilBlob),
		"log_clone_json":             string(logClone),
		"log_clone_equals_auditable": string(logClone) == string(blob),
		// Unlike View's, this projection DOES include the payload — with a TODO beside it.
		"includes_data": a["data"] != nil,
	}
}

// jobFormatMillisAll measures timeutils.FormatMillis, which is `time.UnixMilli(...).Format(...)`
// and therefore reads `time.Local`.
func jobFormatMillisAll() map[string]any {
	zone, offset := time.Now().In(time.Local).Zone()

	values := []int64{
		0,
		1,
		999,
		1000,
		1700000000000, // exactly on a second
		1700000000001, // one millisecond
		1700000000010, // trailing zero in the fraction
		1700000000100, // two trailing zeros
		1700000000123,
		-1,
		-1000,
		-1700000000000,
		253402300799999, // 9999-12-31T23:59:59.999Z
		1234567890123,
	}

	var cases []map[string]any
	for _, v := range values {
		cases = append(cases, map[string]any{
			"millis":    v,
			"formatted": timeutils.FormatMillis(v),
			// The round trip, which is what the YAML codec relies on.
			"round_trips": func() bool {
				back, err := timeutils.ParseFormatedMillis(timeutils.FormatMillis(v))
				return err == nil && back == v
			}(),
		})
	}

	// The SAME instants rendered at a zero offset, which is where the layout's `Z07:00` emits a
	// literal `Z` rather than `+00:00`. The generator pins a +05:30 zone, so without this section
	// nothing exercises that branch — and a UTC server is the common deployment. Formatted through
	// `.UTC()` rather than by changing TZ, so the two sections can sit in one fixture.
	var utcCases []map[string]any
	for _, v := range values {
		utcCases = append(utcCases, map[string]any{
			"millis":    v,
			"formatted": time.UnixMilli(v).UTC().Format(timeutils.RFC3339Milli),
		})
	}

	return map[string]any{
		// The zone this fixture was generated under. The Rust test rebuilds in it ([D-008]).
		"zone_name":      zone,
		"offset_seconds": offset,
		"layout":         timeutils.RFC3339Milli,
		"cases":          cases,
		"utc_cases":      utcCases,
	}
}

func jobParseMillisAll() []map[string]any {
	inputs := []string{
		"",
		"2023-11-14T22:13:20+05:30",
		"2023-11-14T22:13:20.123+05:30",
		"2023-11-14T16:43:20Z",
		"2023-11-14T16:43:20.5Z",
		"2023-11-14T16:43:20.999Z",
		// Four fractional digits — more precision than the layout describes.
		"2023-11-14T16:43:20.9999Z",
		"2023-11-14T16:43:20", // no offset at all
		"2023-11-14",          // date only
		"not a timestamp",
		"2023-13-45T99:99:99Z", // structurally right, semantically impossible
	}

	var out []map[string]any
	for _, in := range inputs {
		millis, err := timeutils.ParseFormatedMillis(in)
		entry := map[string]any{"input": in, "ok": err == nil}
		if err == nil {
			entry["millis"] = millis
		} else {
			entry["error"] = err.Error()
		}
		out = append(out, entry)
	}
	return out
}

func jobWireProbes() []map[string]any {
	probes := []struct {
		name string
		j    model.Job
	}{
		{"zero", model.Job{}},
		{"full", jobValid()},
		{"nil_data", func() model.Job { j := jobValid(); j.Data = nil; return j }()},
		{"empty_data", func() model.Job { j := jobValid(); j.Data = model.StringMap{}; return j }()},
		{"negative_numbers", func() model.Job {
			j := jobValid()
			j.Priority = -1
			j.Progress = -100
			j.CreateAt = -1
			return j
		}()},
		{"escapable_data", func() model.Job {
			j := jobValid()
			j.Data = model.StringMap{"html": "<b>&</b>", "path": "a/b"}
			return j
		}()},
	}

	var out []map[string]any
	for _, p := range probes {
		blob, _ := json.Marshal(p.j)
		out = append(out, map[string]any{
			"name": p.name,
			"json": string(blob),
			"keys": vwKeys(blob),
		})
	}
	return out
}
