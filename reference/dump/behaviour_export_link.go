package main

// Behavioural oracle for the third condition of `ExportLinkProvider.GetCommand`
// (app/slashcommands/command_exportlink.go:45) and `GeneratePresignURLForExport`
// (app/export.go:1272): whether the export backend Go builds at boot is a
// `filestore.FileBackendWithLinkGenerator`. Written to fixtures/behaviour_export_link.json and
// asserted by `mm_app::filestore`'s `export_link_go_parity` test.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-app`'s tests only.
//
// Each row builds the export backend exactly as `PlatformService` does at boot with
// `DedicatedExportStore` on (platform/service.go:398): `SetDefaults`' `FileSettings` with the
// row's `ExportDriverName` (plus the bucket, account and container Go insists on), through
// `NewExportFileBackendSettingsFromConfig` and `NewExportFileBackend`. No backend connects to
// anything when it is built, so no network is needed. A driver Go refuses is a row with
// `constructed: false`: that server does not boot at all.

import (
	"encoding/base64"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/platform/shared/filestore"
)

var exportLinkDrivers = []string{"local", "amazons3", "azureblob", "AmazonS3", "s3", ""}

func writeExportLinkBehaviourFixture(outDir string) error {
	rows := make([]map[string]any, 0, len(exportLinkDrivers))
	for _, driver := range exportLinkDrivers {
		fs := model.FileSettings{}
		fs.SetDefaults(false)
		fs.ExportDriverName = model.NewPointer(driver)
		fs.ExportAmazonS3Bucket = model.NewPointer("bucket")
		fs.ExportAzureStorageAccount = model.NewPointer("account")
		fs.ExportAzureContainer = model.NewPointer("container")
		fs.ExportAzureAccessKey = model.NewPointer(base64.StdEncoding.EncodeToString([]byte("key")))
		settings := filestore.NewExportFileBackendSettingsFromConfig(&fs, false, false, "")
		backend, err := filestore.NewExportFileBackend(settings)
		row := map[string]any{"driver": driver, "constructed": err == nil, "link_generator": false}
		if err == nil {
			_, ok := backend.(filestore.FileBackendWithLinkGenerator)
			row["link_generator"] = ok
			row["driver_name"] = backend.DriverName()
		}
		rows = append(rows, row)
	}
	return writeJSONFixture(outDir, "behaviour_export_link.json", map[string]any{"export_backend": rows})
}
