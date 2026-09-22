//! Port of `App.GenerateSupportPacket` (app/support_packet.go) and
//! `PlatformService.GenerateSupportPacket` (app/platform/support_packet.go) — the files of the
//! zip `GET /api/v4/system/support_packet` streams — and `App.WriteZipFile` (app/file.go:1433).
//!
//! # What each file can be compared on
//!
//! Two servers answering the same request build two packets, and some of a packet is a property
//! of the process that built it. Per file:
//!
//! | file | what is Go's, byte for byte | what is this process's |
//! |---|---|---|
//! | `metadata.yaml` | every key; `server_id`, `license_id`, `customer_id`, `server_version` | `generated_at` |
//! | `stats.yaml` | all of it — counts over the shared database | — |
//! | `jobs.yaml` | all of it, timestamps in the server's zone | — |
//! | `permissions.yaml` | all of it | — |
//! | `plugins.json` | all of it, when both hosts see the same plugins | — |
//! | `database_schema.yaml` | all of it, **as a set of tables**: Go ranges a map | table order |
//! | `diagnostics.yaml` | the key set, the licence, config source, database type/version/schema, file store status and driver, and every probe's status | host figures: pid, start time, descriptors, pool counters, disk space |
//! | `sanitized_config.json` | all of it | — |
//! | `mattermost.log` and the advanced logs | the name; the bytes are the file both servers read | the file grows |
//! | plugin files | name and bytes, whatever the plugin sends | — |
//! | `warning.txt` | the set of lines | the order: Go ranges maps |
//!
//! Go ranges a map of producer functions twice (the six here, the four or six in the platform),
//! so **its file order is random per request** within each half. This port uses the order the
//! Go source declares them in, which is one of the orders Go can produce.
//!
//! # What this process does not write
//!
//! `heap.prof`, `goroutines` and `cpu.prof` are `runtime/pprof` output — the Go runtime's own
//! heap, its goroutine stacks, and five seconds of its CPU. This process has no goroutines and no
//! Go heap, and a file of that name holding anything else would mislead the tool that opens it,
//! so they are absent rather than faked, and the request does not sleep five seconds for a
//! profile it cannot take. [D-980].
//!
//! # `go_version`
//!
//! `runtime.Version()` has no counterpart: this binary is not a Go program. It is written empty
//! — the key stays, so the document's shape is Go's.

use std::io::Write as _;
use std::time::Duration;

use mm_model::config::Config as ModelConfig;
use mm_model::goyaml::{Entry, Node};
use mm_model::license::License;
use mm_model::support_packet::{
    FileData, SUPPORT_PACKET_ERROR_FILE, SupportPacketConfig, SupportPacketDiagnostics,
    SupportPacketJobList, SupportPacketOptions, SupportPacketPermissionInfo,
    SupportPacketPluginList, SupportPacketStats,
};
use mm_model::user_count::UserCountOptions;
use mm_model::utils::AppError;

use crate::App;
use crate::plugin_hooks::HookContext;

/// `envVarInstallType` (platform/support_packet.go:32).
const ENV_VAR_INSTALL_TYPE: &str = "MM_INSTALL_TYPE";
/// `unknownDataPoint` (platform/support_packet.go:33).
const UNKNOWN_DATA_POINT: &str = "unknown";
/// `model.StatusOk`, `StatusFail` and the diagnostics' `StatusDisabled`.
const STATUS_OK: &str = "OK";
const STATUS_FAIL: &str = "FAIL";
const STATUS_DISABLED: &str = "disabled";
/// `model.FileSettingsDefaultDirectory`.
const FILE_SETTINGS_DEFAULT_DIRECTORY: &str = "./data/";
/// `numberOfJobsRuns` (app/support_packet.go:234).
const NUMBER_OF_JOB_RUNS: i64 = 5;
/// The ten-second budget `probeOAuthProvider` and `testPushProxyConnection` give each request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// `io.LimitReader(resp.Body, 1<<20)`.
const PROBE_BODY_LIMIT: usize = 1 << 20;

/// Where the plugin half of the packet comes from — the caller decides, because only the HTTP
/// layer knows whether the Go server beside this one may be running a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketPlugins {
    /// This process hosts the plugins (`MMRS_PLUGIN_HOST=rust`), or plugins are off: ask the
    /// environment, which answers Go's `app.plugin.disabled.app_error` when there is none.
    Environment,
    /// Go hosts them, plugins are on, and its plugin directory holds no bundle: an environment
    /// with nothing in it. Go's answer is the same as an empty environment's.
    NoneRunning,
}

/// Why a licensed request must go to Go whole, decided before anything is read. Each names
/// public code this port does not have yet ([D-981]) or state only the Go process holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportPacketForward {
    /// `FileSettings.DriverName` is not `local`: `TestConnection` and the disk figures belong
    /// to a backend this server does not implement (the file store's own rule).
    FileBackend,
    /// `EmailSettings.SendEmailNotifications` with TLS, STARTTLS or SMTP auth: the probe
    /// `mail.TestConnection` makes needs a TLS client this port does not have.
    SmtpProbe,
}

/// go-multierror's `ListFormatFunc` (multierror/format.go): the text of an `*Error` holding
/// `errors`, which is also what a `Wrap` around one embeds.
pub fn multierror_text(errors: &[String]) -> String {
    if errors.len() == 1 {
        return format!("1 error occurred:\n\t* {}\n\n", errors[0]);
    }
    let points: Vec<String> = errors.iter().map(|e| format!("* {e}")).collect();
    format!(
        "{} errors occurred:\n\t{}\n\n",
        errors.len(),
        points.join("\n\t")
    )
}

/// `Filename` and `Body` with the file's name.
fn file(name: &str, body: impl Into<Vec<u8>>) -> FileData {
    FileData {
        filename: name.to_owned(),
        body: body.into(),
    }
}

/// Port of `supportPacketFileName` (api4/system.go:141):
/// `mm_support_packet_<SanitizeFileName(company)>_<2006-01-02T15-04>.zip`, in the server's zone.
pub fn support_packet_file_name<Tz: chrono::TimeZone>(
    now: &chrono::DateTime<Tz>,
    customer_name: &str,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!(
        "mm_support_packet_{}_{}.zip",
        mm_model::utils::sanitize_file_name(customer_name),
        now.format("%Y-%m-%dT%H-%M")
    )
}

impl App {
    /// The forward decision for a licensed request, from the configuration alone — see
    /// [`SupportPacketForward`]. `None` means every file can be built here.
    pub fn support_packet_forward(config: &ModelConfig) -> Option<SupportPacketForward> {
        let driver = config.file_settings.driver_name.as_deref().unwrap_or("");
        if driver != crate::filestore::DRIVER_LOCAL {
            return Some(SupportPacketForward::FileBackend);
        }
        let email = &config.email_settings;
        if email.send_email_notifications.unwrap_or(false) {
            let security = email.connection_security.as_deref().unwrap_or("");
            if security == SMTP_TLS
                || security == SMTP_STARTTLS
                || email.enable_smtp_auth.unwrap_or(false)
            {
                return Some(SupportPacketForward::SmtpProbe);
            }
        }
        None
    }

    /// Port of `App.GenerateSupportPacket` (app/support_packet.go:22): the six app files, the
    /// platform's, the plugins' and — when anything failed — `warning.txt`, in that order.
    ///
    /// Nothing here fails the request: each producer's error becomes a line of `warning.txt`,
    /// and a producer that failed outright contributes no file. Go runs the two halves on a
    /// `WaitGroup` only so that a cluster's nodes can be asked concurrently; with no cluster
    /// (every build this project can run — `a.Cluster()` is a nil interface) the one goroutine
    /// is the whole of it, and running it inline is the same.
    #[tracing::instrument(skip_all, fields(files, warnings))]
    pub async fn generate_support_packet(
        &self,
        ctx: &HookContext,
        options: &SupportPacketOptions,
        plugins: PacketPlugins,
        config: &ModelConfig,
    ) -> Vec<FileData> {
        let license = self.license().await.ok().flatten();
        let mut files = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        let produced = [
            self.support_packet_metadata(license.as_deref()).await,
            self.support_packet_stats().await,
            self.support_packet_job_list().await,
            self.support_packet_permissions_info().await,
            self.support_packet_plugins_file(plugins),
            self.support_packet_database_schema(config).await,
        ];
        for (name, (data, errors)) in [
            "metadata",
            "stats",
            "jobs",
            "permissions",
            "plugins",
            "schema",
        ]
        .iter()
        .zip(produced)
        {
            for error in &errors {
                tracing::error!(file = name, error = %error, "Failed to generate file for Support Packet");
            }
            warnings.extend(errors);
            files.extend(data);
        }

        let (platform_files, platform_errors) = self
            .platform_support_packet(options, plugins, config, license.as_deref())
            .await;
        warnings.extend(platform_errors);
        files.extend(platform_files);

        let (plugin_files, plugin_errors) = self
            .run_generate_support_data(ctx, &options.plugin_packets)
            .await;
        warnings.extend(plugin_errors);
        files.extend(plugin_files);

        if !warnings.is_empty() {
            files.push(file(SUPPORT_PACKET_ERROR_FILE, multierror_text(&warnings)));
        }
        let span = tracing::Span::current();
        span.record("files", files.len());
        span.record("warnings", warnings.len());
        files
    }

    /// Go's `AppError.Error()` for an error this server built: translated first, as
    /// `NewAppError` does at construction, then rendered.
    fn app_error_text(&self, mut err: Box<AppError>) -> String {
        if let Some(bundle) = crate::i18n::loaded() {
            bundle.translate_app_error(
                bundle.server_locale(&self.config().default_server_locale),
                &mut err,
            );
        }
        err.to_string()
    }

    /// Port of `getSupportPacketMetadata` (app/support_packet.go:357).
    async fn support_packet_metadata(
        &self,
        license: Option<&License>,
    ) -> (Option<FileData>, Vec<String>) {
        let server_id = self.telemetry_id().await;
        match mm_model::packet_metadata::generate_packet_metadata(
            mm_model::packet_metadata::PacketType::from(
                mm_model::packet_metadata::PacketType::SUPPORT_PACKET,
            ),
            &server_id,
            license,
            None,
        ) {
            Ok(metadata) => (
                Some(file(
                    mm_model::packet_metadata::PACKET_METADATA_FILE_NAME,
                    metadata.to_yaml(),
                )),
                Vec::new(),
            ),
            Err(err) => (
                None,
                vec![format!(
                    "failed to generate Packet metadata: invalid metadata: {err}"
                )],
            ),
        }
    }

    /// Port of `getSupportPacketStats` (app/support_packet.go:139).
    ///
    /// **Only the last failure is reported.** Every branch is `rErr = multierror.Append(err)`
    /// with the error as the *first* argument, which starts a new list rather than adding to
    /// the old one — so a database outage that fails all fourteen counts reports the outgoing
    /// webhook count alone. The file is written whatever failed, with zeros where it did.
    async fn support_packet_stats(&self) -> (Option<FileData>, Vec<String>) {
        use mm_store::channel_store::ChannelStore as _;
        use mm_store::command_store::CommandStore as _;
        use mm_store::post_store::PostStore as _;
        use mm_store::team_store::TeamStore as _;
        use mm_store::user_store::UserStore as _;
        use mm_store::webhook_store::WebhookStore as _;

        let store = self.store();
        let mut last: Option<String> = None;
        let mut take = |result: Result<i64, mm_store::StoreError>, what: &str| match result {
            Ok(value) => value,
            Err(err) => {
                last = Some(format!("{what}: {err}"));
                0
            }
        };
        let active = UserCountOptions {
            include_bot_accounts: false,
            include_deleted: false,
            ..UserCountOptions::default()
        };
        let mut stats = SupportPacketStats {
            registered_users: take(
                store
                    .user()
                    .count(&UserCountOptions {
                        include_deleted: true,
                        ..UserCountOptions::default()
                    })
                    .await,
                "failed to get registered user count",
            ),
            active_users: take(
                store.user().count(&UserCountOptions::default()).await,
                "failed to get active user count",
            ),
            daily_active_users: take(
                store
                    .user()
                    .analytics_active_count(crate::analytics::DAY_MILLISECONDS, &active)
                    .await,
                "failed to get daily active user count",
            ),
            monthly_active_users: take(
                store
                    .user()
                    .analytics_active_count(crate::analytics::MONTH_MILLISECONDS, &active)
                    .await,
                "failed to get monthly active user count",
            ),
            deactivated_users: take(
                store.user().analytics_get_inactive_users_count().await,
                "failed to get deactivated user count",
            ),
            guests: take(
                store.user().analytics_get_guest_count().await,
                "failed to get guest count",
            ),
            single_channel_guests: take(
                store
                    .user()
                    .analytics_get_single_channel_guest_count()
                    .await,
                "failed to get single channel guest count",
            ),
            bot_accounts: take(
                store
                    .user()
                    .count(&UserCountOptions {
                        include_bot_accounts: true,
                        exclude_regular_users: true,
                        ..UserCountOptions::default()
                    })
                    .await,
                "failed to get bot acount count",
            ),
            posts: take(
                store.post().analytics_post_count_total().await,
                "failed to get post count",
            ),
            ..SupportPacketStats::default()
        };
        // `AnalyticsTypeCount("", "O")` plus `AnalyticsTypeCount("", "P")`: one grouped query
        // answers both, and when it fails both of Go's fail — the later message is the one kept.
        match store.channel().analytics_count_all("").await {
            Ok(counts) => {
                stats.channels = counts
                    .get(mm_model::channel::CHANNEL_TYPE_OPEN)
                    .copied()
                    .unwrap_or(0)
                    + counts
                        .get(mm_model::channel::CHANNEL_TYPE_PRIVATE)
                        .copied()
                        .unwrap_or(0);
            }
            Err(err) => last = Some(format!("failed to get private channels count: {err}")),
        }
        let mut take = |result: Result<i64, mm_store::StoreError>, what: &str| match result {
            Ok(value) => value,
            Err(err) => {
                last = Some(format!("{what}: {err}"));
                0
            }
        };
        stats.teams = take(
            store
                .team()
                .analytics_team_count(&mm_model::team_search::TeamSearch {
                    include_deleted: Some(false),
                    ..Default::default()
                })
                .await,
            "failed to get team count",
        );
        stats.slash_commands = take(
            store.command().analytics_command_count("").await,
            "failed to get command count",
        );
        stats.incoming_webhooks = take(
            store.webhook().analytics_incoming_count("", "").await,
            "failed to get incoming webhook count",
        );
        stats.outgoing_webhooks = take(
            store.webhook().analytics_outgoing_count("").await,
            "failed to get  outgoing webhook count",
        );
        (
            Some(file("stats.yaml", stats.to_yaml())),
            last.into_iter().collect(),
        )
    }

    /// Port of `getSupportPacketJobList` (app/support_packet.go:233): the five newest runs of
    /// six enterprise job types. Same "only the last failure" rule as the stats.
    async fn support_packet_job_list(&self) -> (Option<FileData>, Vec<String>) {
        use mm_store::job_store::JobStore as _;

        let mut last: Option<String> = None;
        let mut jobs = SupportPacketJobList::default();
        for (job_type, slot, what) in [
            (
                mm_model::job::JOB_TYPE_LDAP_SYNC,
                &mut jobs.ldap_sync_jobs,
                "error while getting LDAP sync jobs",
            ),
            (
                mm_model::job::JOB_TYPE_DATA_RETENTION,
                &mut jobs.data_retention_jobs,
                "error while getting data retention jobs",
            ),
            (
                mm_model::job::JOB_TYPE_MESSAGE_EXPORT,
                &mut jobs.message_export_jobs,
                "error while getting message export jobs",
            ),
            (
                mm_model::job::JOB_TYPE_ELASTICSEARCH_POST_INDEXING,
                &mut jobs.elastic_post_indexing_jobs,
                "error while getting ES post indexing jobs",
            ),
            (
                mm_model::job::JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION,
                &mut jobs.elastic_post_aggregation_jobs,
                "error while getting ES post aggregation jobs",
            ),
            (
                mm_model::job::JOB_TYPE_MIGRATIONS,
                &mut jobs.migration_jobs,
                "error while getting migration jobs",
            ),
        ] {
            match self
                .store()
                .job()
                .get_all_by_type_page(job_type, 0, NUMBER_OF_JOB_RUNS)
                .await
            {
                Ok(found) => *slot = found,
                Err(err) => last = Some(format!("{what}: {err}")),
            }
        }
        (
            Some(file("jobs.yaml", jobs.to_yaml())),
            last.into_iter().collect(),
        )
    }

    /// Port of `getSupportPacketPermissionsInfo` (app/support_packet.go:279): every scheme, a
    /// hundred at a time, and every role (with channel-scheme permissions merged down, as
    /// `GetAllRoles` does), each `Sanitize`d — display name and description, and a scheme's
    /// name too. Same "only the last failure" rule; a failing schemes page ends the loop.
    async fn support_packet_permissions_info(&self) -> (Option<FileData>, Vec<String>) {
        const PER_PAGE: i64 = 100;
        let mut last: Option<String> = None;
        let mut info = SupportPacketPermissionInfo::default();
        let mut page = 0;
        loop {
            match self.get_schemes_page("", page, PER_PAGE).await {
                Ok(schemes) => {
                    let short = (schemes.len() as i64) < PER_PAGE;
                    info.schemes.extend(schemes);
                    if short {
                        break;
                    }
                    page += 1;
                }
                Err(err) => {
                    last = Some(format!(
                        "failed to get list of schemes: {}",
                        self.app_error_text(err)
                    ));
                    break;
                }
            }
        }
        info.schemes
            .iter_mut()
            .for_each(mm_model::scheme::Scheme::sanitize);
        match self.get_all_roles().await {
            Ok(roles) => info.roles = roles,
            Err(err) => {
                last = Some(format!(
                    "failed to get list of roles: {}",
                    self.app_error_text(err)
                ))
            }
        }
        info.roles
            .iter_mut()
            .for_each(mm_model::role::Role::sanitize);
        (
            Some(file("permissions.yaml", info.to_yaml())),
            last.into_iter().collect(),
        )
    }

    /// Port of `getPluginsFile` (app/support_packet.go:330): every available plugin's manifest,
    /// split by whether it runs, through `json.MarshalIndent` — `null` for a side with none.
    fn support_packet_plugins_file(
        &self,
        plugins: PacketPlugins,
    ) -> (Option<FileData>, Vec<String>) {
        let response = match plugins {
            PacketPlugins::NoneRunning => mm_model::manifest::PluginsResponse::default(),
            PacketPlugins::Environment => match self.get_plugins() {
                Ok(response) => response,
                Err(err) => {
                    return (
                        None,
                        vec![format!(
                            "failed to get plugin list for Support Packet: {}",
                            self.app_error_text(err)
                        )],
                    );
                }
            },
        };
        let collect = |infos: Option<Vec<mm_model::manifest::PluginInfo>>| {
            let manifests: Vec<_> = infos
                .unwrap_or_default()
                .into_iter()
                .map(|info| info.manifest)
                .collect();
            (!manifests.is_empty()).then_some(manifests)
        };
        let list = SupportPacketPluginList {
            enabled: collect(response.active),
            disabled: collect(response.inactive),
        };
        match mm_model::utils::go_json_marshal_indent(&list) {
            Ok(body) => (Some(file("plugins.json", body)), Vec::new()),
            Err(err) => (
                None,
                vec![format!("failed to marshal plugin list into json: {err}")],
            ),
        }
    }

    /// Port of `getSupportPacketDatabaseSchema` (app/support_packet.go:375). Not PostgreSQL is
    /// no file and no error; **any** failure of the dump is no file either — Go discards the
    /// partial schema it was handed along with the error.
    async fn support_packet_database_schema(
        &self,
        config: &ModelConfig,
    ) -> (Option<FileData>, Vec<String>) {
        if config.sql_settings.driver_name.as_deref()
            != Some(mm_model::config::DATABASE_DRIVER_POSTGRES)
        {
            return (None, Vec::new());
        }
        let (schema, errors) = self.store().get_schema_definition().await;
        if !errors.is_empty() {
            return (
                None,
                vec![format!(
                    "failed to get schema definition: {}",
                    multierror_text(&errors)
                )],
            );
        }
        (
            Some(file("database_schema.yaml", schema.to_yaml())),
            Vec::new(),
        )
    }

    /// Port of `PlatformService.GenerateSupportPacket` (platform/support_packet.go:83): the
    /// diagnostics, the sanitised configuration and — unless `basic_server_logs=false` — the log
    /// file and every advanced-logging file target, with the errors of all of them.
    async fn platform_support_packet(
        &self,
        options: &SupportPacketOptions,
        plugins: PacketPlugins,
        config: &ModelConfig,
        license: Option<&License>,
    ) -> (Vec<FileData>, Vec<String>) {
        let mut files = Vec::new();
        let mut errors = Vec::new();

        let (diagnostics, diagnostics_errors) =
            self.support_packet_diagnostics(config, license).await;
        files.push(diagnostics);
        errors.extend(diagnostics_errors);

        files.push(self.sanitized_config_file(config, plugins));

        if options.include_logs {
            match self.support_packet_log_file().await {
                Ok(log) => files.push(log),
                Err(err) => {
                    tracing::error!(file = "mattermost log", error = %err, "Failed to generate file for Support Packet");
                    errors.push(err);
                }
            }
            let (advanced, advanced_errors) = self.advanced_log_files(config).await;
            files.extend(advanced);
            errors.extend(advanced_errors);
        }
        (files, errors)
    }

    /// Port of `getSanitizedConfigFile` (platform/support_packet.go:536) and `getSanitizedConfig`
    /// (platform/config.go:45): the running configuration with every secret replaced, the data
    /// sources **partially** redacted (`PartiallyRedactDataSources: true` — user and password
    /// only, where `GET /config` hides the whole DSN), the plugin settings sanitised against the
    /// installed manifests, and the feature flags as a trailing top-level key.
    fn sanitized_config_file(&self, config: &ModelConfig, plugins: PacketPlugins) -> FileData {
        let mut sanitized = config.clone();
        let manifests: Option<Vec<mm_model::manifest::Manifest>> = match plugins {
            PacketPlugins::NoneRunning => Some(Vec::new()),
            PacketPlugins::Environment => self.plugins_environment().and_then(|environment| {
                environment
                    .available()
                    .ok()
                    .map(|bundles| bundles.into_iter().filter_map(|b| b.manifest).collect())
            }),
        };
        if manifests.is_none() {
            tracing::warn!(
                "Failed to get plugin manifests for config sanitization. Will sanitize all plugin settings."
            );
        }
        crate::config::sanitize_with(&mut sanitized, manifests.as_deref(), true);
        let packet = SupportPacketConfig::new(sanitized);
        let body = mm_model::utils::go_json_marshal_indent(&packet).unwrap_or_else(|err| {
            tracing::error!(error = %err, "failed to sanitized config into json");
            String::new()
        });
        file("sanitized_config.json", body)
    }

    /// Port of `PlatformService.GetLogFile` (platform/log.go:202) for the packet: the whole file,
    /// under `config.LogFilename`.
    async fn support_packet_log_file(&self) -> Result<FileData, String> {
        let path = self.get_log_file().await.map_err(|err| err.to_string())?;
        let body = tokio::fs::read(&path).await.map_err(|source| {
            crate::logs::LogFileError::Read {
                path: path.to_string_lossy().into_owned(),
                source,
            }
            .to_string()
        })?;
        Ok(file(crate::logs::LOG_FILENAME, body))
    }

    /// Port of `PlatformService.GetAdvancedLogs` (platform/log.go:252): every `file` target of
    /// the two advanced-logging documents, read whole under its base name.
    async fn advanced_log_files(&self, config: &ModelConfig) -> (Vec<FileData>, Vec<String>) {
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for (name, logging) in [
            (
                "LogSettings.AdvancedLoggingJSON",
                &config.log_settings.advanced_logging_json,
            ),
            (
                "ExperimentalAuditSettings.AdvancedLoggingJSON",
                &config.experimental_audit_settings.advanced_logging_json,
            ),
        ] {
            let targets = match advanced_logging_targets(logging) {
                Ok(targets) => targets,
                Err(err) => {
                    errors.push(format!(
                        "error decoding advanced logging configuration {name}: {err}"
                    ));
                    continue;
                }
            };
            for target in targets {
                let filename = match target {
                    Ok(filename) => filename,
                    Err(err) => {
                        errors.push(format!(
                            "error decoding file target options in {name}: {err}"
                        ));
                        continue;
                    }
                };
                let path = std::path::PathBuf::from(&filename);
                if let Err(reason) =
                    crate::logs::validate_log_file_path(&path, &crate::logs::get_log_root_path())
                {
                    tracing::error!(path = %filename, config_section = name, error = %reason, "Blocked attempt to read log file outside allowed root");
                    errors.push(format!(
                        "log file path {filename} in {name} is outside allowed logging directory: {reason}"
                    ));
                    continue;
                }
                match tokio::fs::read(&path).await {
                    Ok(body) => files.push(file(&mm_model::go_path::base(&filename), body)),
                    Err(err) => errors.push(format!(
                        "failed to read advanced log file at path {filename} in {name}: {}",
                        go_os_error("open", &filename, &err)
                    )),
                }
            }
        }
        (files, errors)
    }
}

/// `utils.IsEmptyJSON` then `json.Unmarshal` into `mlog.LoggerConfiguration`: the `file`
/// targets' `filename` options, each as `Ok(name)` or the options' own decode error. Go's `null`
/// is not "empty" to `IsEmptyJSON` but unmarshals into an empty map, so it has no targets either.
fn advanced_logging_targets(
    logging: &serde_json::Value,
) -> Result<Vec<Result<String, String>>, String> {
    use serde_json::Value;
    let object = match logging {
        Value::Null => return Ok(Vec::new()),
        Value::String(s) if s.is_empty() => return Ok(Vec::new()),
        Value::Array(a) if a.is_empty() => return Ok(Vec::new()),
        Value::Object(o) => o,
        Value::String(_) => {
            return Err(
                "json: cannot unmarshal string into Go value of type mlog.LoggerConfiguration"
                    .to_owned(),
            );
        }
        Value::Array(_) => {
            return Err(
                "json: cannot unmarshal array into Go value of type mlog.LoggerConfiguration"
                    .to_owned(),
            );
        }
        Value::Bool(_) => {
            return Err(
                "json: cannot unmarshal bool into Go value of type mlog.LoggerConfiguration"
                    .to_owned(),
            );
        }
        Value::Number(_) => {
            return Err(
                "json: cannot unmarshal number into Go value of type mlog.LoggerConfiguration"
                    .to_owned(),
            );
        }
    };
    let field = |target: &serde_json::Map<String, Value>, name: &str| {
        target
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for target in object.values() {
        let Some(target) = target.as_object() else {
            continue;
        };
        if field(target, "type")
            .and_then(|t| t.as_str().map(str::to_owned))
            .as_deref()
            != Some("file")
        {
            continue;
        }
        out.push(match field(target, "options") {
            None => Err("unexpected end of JSON input".to_owned()),
            Some(Value::Null) => Ok(String::new()),
            Some(Value::Object(options)) => Ok(field(&options, "filename")
                .and_then(|f| f.as_str().map(str::to_owned))
                .unwrap_or_default()),
            Some(_) => Err(
                "json: cannot unmarshal into Go value of type struct { Filename string \"json:\\\"filename\\\"\" }"
                    .to_owned(),
            ),
        });
    }
    Ok(out)
}

/// Go's `*fs.PathError` text for an `os` call: `open <path>: <reason>`, with the reason in
/// Go's words for the errors a support packet meets.
fn go_os_error(op: &str, path: &str, err: &std::io::Error) -> String {
    let reason = match err.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        std::io::ErrorKind::IsADirectory => "is a directory".to_owned(),
        _ => err.to_string(),
    };
    format!("{op} {path}: {reason}")
}

// ---------------------------------------------------------------------------------------------
// diagnostics.yaml
// ---------------------------------------------------------------------------------------------

/// `diagnosticsYAMLComments` (platform/support_packet.go:40), as (path, head, line). Pinned
/// against goccy's output by `go_parity::diagnostics_match_goccy`.
const DIAGNOSTICS_COMMENTS: [(&str, Option<&str>, Option<&str>); 32] = [
    ("server.os", Some(" Machine"), None),
    (
        "server.cpu_cores",
        Some(" Capacity (hardware → effective quota)"),
        Some(" logical CPUs visible to the OS"),
    ),
    (
        "server.total_memory_mb",
        None,
        Some(" host/VM total RAM; may exceed container limit"),
    ),
    (
        "server.container_cpu_limit",
        None,
        Some(" cgroup v2 CPU quota in CPUs; Linux only, omitted if no limit set"),
    ),
    (
        "server.container_memory_limit_mb",
        None,
        Some(" cgroup v2 memory quota in MB; Linux only, omitted if no limit set"),
    ),
    ("server.process_id", Some(" Process lifecycle"), None),
    (
        "server.started_at",
        None,
        Some(" when Mattermost process started"),
    ),
    (
        "server.host_started_at",
        None,
        Some(" when the host OS booted; omitted if unavailable"),
    ),
    (
        "server.open_file_descriptors",
        None,
        Some(" current open FDs for this process"),
    ),
    (
        "server.max_file_descriptors",
        None,
        Some(" system limit (ulimit -n)"),
    ),
    ("server.version", Some(" Software"), None),
    (
        "database.master_pool_wait_count",
        None,
        Some(" cumulative; total times a goroutine waited for a connection since process start"),
    ),
    (
        "database.master_pool_wait_duration_ms",
        None,
        Some(" cumulative wait time across all goroutines since process start"),
    ),
    (
        "database.master_connections_closed_max_idle",
        None,
        Some(" cumulative; connections closed because the idle pool was full"),
    ),
    (
        "database.master_connections_closed_max_lifetime",
        None,
        Some(" cumulative; connections closed for exceeding ConnMaxLifetime"),
    ),
    (
        "database.replica_pool_wait_count",
        None,
        Some(" cumulative across all replicas; see master_pool_wait_count"),
    ),
    (
        "database.replica_pool_wait_duration_ms",
        None,
        Some(" cumulative across all replicas"),
    ),
    (
        "database.replica_connections_closed_max_idle",
        None,
        Some(" cumulative across all replicas"),
    ),
    (
        "database.replica_connections_closed_max_lifetime",
        None,
        Some(" cumulative across all replicas"),
    ),
    (
        "database.cache_hit_ratio",
        Some(" PostgreSQL-only (these fields are omitted on MySQL)"),
        Some(
            " blks_hit / (blks_hit + blks_read) from pg_stat_database; cumulative since stats reset",
        ),
    ),
    (
        "database.deadlocks",
        None,
        Some(" cumulative since pg_stat_database reset"),
    ),
    (
        "database.temp_files",
        None,
        Some(" cumulative count of temp files created since stats reset"),
    ),
    (
        "database.temp_bytes_mb",
        None,
        Some(" cumulative bytes written to temp files, in MB"),
    ),
    (
        "database.rollbacks",
        None,
        Some(" cumulative transaction rollbacks since stats reset"),
    ),
    (
        "database.idle_in_transaction_count",
        None,
        Some(" point-in-time count from pg_stat_activity"),
    ),
    (
        "database.longest_query_duration_seconds",
        None,
        Some(" point-in-time; max age of any active query right now"),
    ),
    (
        "database.waiting_for_lock_count",
        None,
        Some(" point-in-time count of backends waiting on a Lock wait_event_type"),
    ),
    (
        "database.posts_dead_tuples",
        None,
        Some(" n_dead_tup for the posts table from pg_stat_user_tables"),
    ),
    (
        "database.posts_last_autovacuum",
        None,
        Some(" last autovacuum on posts; null if never autovacuumed (then omitted)"),
    ),
    (
        "file_store.filesystem_type",
        None,
        Some(" local driver only (e.g. ext4, xfs); omitted for s3 and other remote drivers"),
    ),
    (
        "file_store.total_mb",
        None,
        Some(" local driver only; capacity of the volume hosting FileSettings.Directory"),
    ),
    (
        "file_store.available_mb",
        None,
        Some(" local driver only; free space remaining on that volume"),
    ),
];

/// `yaml.MarshalWithOptions(&d, yaml.WithComment(diagnosticsYAMLComments))`. A path whose key the
/// document omits is skipped, as goccy skips a path its filter does not find.
pub fn diagnostics_yaml(d: &SupportPacketDiagnostics) -> String {
    let mut doc = d.to_yaml_node();
    for (path, head, line) in DIAGNOSTICS_COMMENTS {
        if let Some(entry) = find_entry(&mut doc, path) {
            if let Some(head) = head {
                entry.head.push(head.to_owned());
            }
            if let Some(line) = line {
                entry.line = Some(line.to_owned());
            }
        }
    }
    mm_model::goyaml::marshal(&doc)
}

fn find_entry<'a>(node: &'a mut Node, path: &str) -> Option<&'a mut Entry> {
    let (first, rest) = match path.split_once('.') {
        Some((first, rest)) => (first, Some(rest)),
        None => (path, None),
    };
    let Node::Map(entries) = node else {
        return None;
    };
    let entry = entries.iter_mut().find(|e| e.key == first)?;
    match rest {
        None => Some(entry),
        Some(rest) => find_entry(&mut entry.value, rest),
    }
}

/// `mail.TLS` and `mail.StartTLS` (platform/shared/mail/mail.go:25).
const SMTP_TLS: &str = "TLS";
const SMTP_STARTTLS: &str = "STARTTLS";

impl App {
    /// Port of `getSupportPacketDiagnostics` (platform/support_packet.go:153).
    ///
    /// # The error list restarts, in Go's places
    ///
    /// Most failures are `multierror.Append(rErr, err)`, but five — the hostname, the database
    /// type and schema version, the database version, the disk and the marshal — are
    /// `multierror.Append(err)`, which **discards** everything gathered before. Reproduced: a
    /// packet whose descriptor count and disk both failed reports only the disk.
    ///
    /// # The nil interfaces
    ///
    /// `LdapDiagnostic()`, `SamlDiagnostic()` and `SearchEngine.ElasticsearchEngine` are
    /// implemented only in the private enterprise tree, so on every build this project runs they
    /// are nil and the three sections say `disabled` whatever the settings say — the SAML
    /// provider type, a string test on `IdpDescriptorURL`, is the one part computed. So is the
    /// cluster section, which a nil cluster leaves at its zero value.
    async fn support_packet_diagnostics(
        &self,
        config: &ModelConfig,
        license: Option<&License>,
    ) -> (FileData, Vec<String>) {
        let mut errors: Vec<String> = Vec::new();
        let mut d = SupportPacketDiagnostics {
            version: mm_model::support_packet::CURRENT_SUPPORT_PACKET_VERSION,
            ..SupportPacketDiagnostics::default()
        };

        if let Some(license) = license {
            d.license.company = license
                .customer
                .as_ref()
                .map(|c| c.company.clone())
                .unwrap_or_default();
            d.license.users = license.features.as_ref().and_then(|f| f.users).unwrap_or(0);
            d.license.sku_short_name = license.sku_short_name.clone();
            d.license.is_trial = license.is_trial;
            d.license.is_gov_sku = license.is_gov_sku;
            d.license.is_non_production = license.is_non_production;
        }

        let s = &mut d.server;
        s.os = std::env::consts::OS.to_owned();
        s.architecture = machine::go_arch().to_owned();
        s.cpu_cores = std::thread::available_parallelism().map_or(1, |n| n.get() as i64);
        match machine::total_memory() {
            Ok(bytes) => s.total_memory_mb = bytes / 1024 / 1024,
            Err(err) => errors.push(format!("error while getting total memory: {err}")),
        }
        match machine::container_limits() {
            Ok((cpu, memory_mb)) => {
                s.container_cpu_limit = cpu;
                s.container_memory_limit_mb = memory_mb;
            }
            Err(err) => {
                tracing::debug!(error = %err, "Failed to get container limits for Support Packet")
            }
        }
        match machine::hostname() {
            Ok(hostname) => s.hostname = hostname,
            Err(err) => errors = vec![format!("error while getting hostname: {err}")],
        }
        s.process_id = i64::from(std::process::id());
        s.started_at = Some(machine::process_started_at());
        if let Ok(uptime) = machine::host_uptime_seconds() {
            s.host_started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(uptime));
        }
        s.version = mm_model::version::CURRENT_VERSION.to_owned();
        s.build_hash = mm_model::version::BUILD_HASH.to_owned();
        s.installation_type = std::env::var(ENV_VAR_INSTALL_TYPE)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| UNKNOWN_DATA_POINT.to_owned());
        match machine::open_file_descriptors() {
            Ok(n) => s.open_file_descriptors = n,
            Err(err) => {
                s.open_file_descriptors = -1;
                errors.push(format!(
                    "error while getting open file descriptor count: {err}"
                ));
            }
        }
        match machine::max_file_descriptors() {
            Ok(n) => s.max_file_descriptors = n,
            Err(err) => {
                s.max_file_descriptors = -1;
                errors.push(format!(
                    "error while getting max file descriptor limit: {err}"
                ));
            }
        }

        d.config.source = describe_config();

        let driver = config.sql_settings.driver_name.clone().unwrap_or_default();
        match self.store().get_db_schema_version().await {
            Ok(version) => {
                d.database.type_ = driver;
                d.database.schema_version = version.to_string();
            }
            Err(err) => {
                errors = vec![format!(
                    "error while getting DB type and schema version: {err}"
                )]
            }
        }
        match self.store().get_db_version(false).await {
            Ok(version) => d.database.version = version,
            Err(err) => errors = vec![format!("error while getting DB version: {err}")],
        }
        d.database.master_connections = self.store().total_master_db_connections();
        d.database.replica_connections = self.store().total_read_db_connections();
        d.database.search_connections = 0;
        let (store_diagnostics, store_errors) = self.store().get_diagnostics().await;
        apply_store_diagnostics(&mut d, store_diagnostics);
        if !store_errors.is_empty() {
            errors.push(format!(
                "error while collecting support packet database diagnostics: {}",
                multierror_text(&store_errors)
            ));
        }

        let backend = self.file_backend();
        match backend.test_connection().await {
            Ok(()) => d.file_store.status = STATUS_OK.to_owned(),
            Err(err) => {
                d.file_store.status = STATUS_FAIL.to_owned();
                d.file_store.error = err.to_string();
            }
        }
        d.file_store.driver = backend.driver_name().to_owned();
        if d.file_store.driver == crate::filestore::DRIVER_LOCAL {
            let dir = config
                .file_settings
                .directory
                .clone()
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| FILE_SETTINGS_DEFAULT_DIRECTORY.to_owned());
            match machine::disk_info(&dir).await {
                Ok(info) => {
                    d.file_store.filesystem_type = info.filesystem_type;
                    d.file_store.total_mb = info.total_mb;
                    d.file_store.available_mb = info.available_mb;
                }
                Err(err) => errors = vec![format!("error while getting disk space info: {err}")],
            }
        }

        d.websocket.connections = self.hub().conn_count() as i64;

        d.ldap.status = STATUS_DISABLED.to_owned();

        if let Some(url) = config
            .saml_settings
            .idp_descriptor_url
            .as_deref()
            .filter(|u| !u.is_empty())
        {
            d.saml.provider_type = detect_saml_provider_type(url).to_owned();
        }
        d.saml.status = STATUS_DISABLED.to_owned();

        d.elastic_search.status = STATUS_DISABLED.to_owned();

        let email = &config.email_settings;
        if email.send_email_notifications.unwrap_or(false) {
            let site_url = config.service_settings.site_url.as_deref().unwrap_or("");
            let probe = SmtpProbe {
                hostname: hostname_from_site_url(site_url),
                server: email.smtp_server.clone().unwrap_or_default(),
                port: email.smtp_port.clone().unwrap_or_default(),
                timeout_secs: email.smtp_server_timeout.unwrap_or(0),
            };
            match probe.test_connection().await {
                Ok(()) => d.notifications.email.status = STATUS_OK.to_owned(),
                Err(err) => {
                    d.notifications.email.status = STATUS_FAIL.to_owned();
                    d.notifications.email.error = err;
                }
            }
        } else {
            d.notifications.email.status = STATUS_DISABLED.to_owned();
        }

        d.oauth_providers.gitlab = probe_oauth_provider(sso_of(&config.git_lab_settings)).await;
        d.oauth_providers.google = probe_oauth_provider(sso_of(&config.google_settings)).await;
        d.oauth_providers.office365 = probe_oauth_provider(SsoProbe {
            enable: config.office365_settings.enable.unwrap_or(false),
            discovery_endpoint: config
                .office365_settings
                .discovery_endpoint
                .clone()
                .unwrap_or_default(),
            token_endpoint: config
                .office365_settings
                .token_endpoint
                .clone()
                .unwrap_or_default(),
        })
        .await;
        d.oauth_providers.openid = probe_oauth_provider(sso_of(&config.open_id_settings)).await;

        if email.send_push_notifications.unwrap_or(false) {
            let server = email.push_notification_server.clone().unwrap_or_default();
            match test_push_proxy_connection(&server).await {
                Ok(()) => d.notifications.push.status = STATUS_OK.to_owned(),
                Err(err) => {
                    d.notifications.push.status = STATUS_FAIL.to_owned();
                    d.notifications.push.error = err;
                }
            }
        } else {
            d.notifications.push.status = STATUS_DISABLED.to_owned();
        }

        (file("diagnostics.yaml", diagnostics_yaml(&d)), errors)
    }
}

/// `ps.DescribeConfig()` — `DatabaseStore.String()` (config/database.go:320): the configuration
/// store's DSN through `SanitizeDataSource`. This process reads its configuration from the
/// database `DATABASE_URL` names; `MM_CONFIG`, when set, is Go's own name for the same thing
/// and wins. A failure is the empty string, as Go discards the error.
fn describe_config() -> String {
    let dsn = std::env::var("MM_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .unwrap_or_default();
    mm_model::config::sanitize_data_source(mm_model::config::DATABASE_DRIVER_POSTGRES, &dsn)
        .unwrap_or_default()
}

/// Port of `applyStoreDiagnostics` (platform/support_packet.go:393): every counter copied,
/// whatever failed.
fn apply_store_diagnostics(
    d: &mut SupportPacketDiagnostics,
    s: mm_store::support_packet_store::DatabaseDiagnostics,
) {
    let db = &mut d.database;
    db.master_connections_in_use = s.master_connections_in_use;
    db.master_connections_idle = s.master_connections_idle;
    db.master_pool_wait_count = s.master_pool_wait_count;
    db.master_pool_wait_duration_ms = s.master_pool_wait_duration_ms;
    db.master_connections_closed_max_idle = s.master_connections_closed_max_idle;
    db.master_connections_closed_max_lifetime = s.master_connections_closed_max_lifetime;
    db.replica_connections_in_use = s.replica_connections_in_use;
    db.replica_connections_idle = s.replica_connections_idle;
    db.replica_pool_wait_count = s.replica_pool_wait_count;
    db.replica_pool_wait_duration_ms = s.replica_pool_wait_duration_ms;
    db.replica_connections_closed_max_idle = s.replica_connections_closed_max_idle;
    db.replica_connections_closed_max_lifetime = s.replica_connections_closed_max_lifetime;
    db.cache_hit_ratio = s.cache_hit_ratio;
    db.deadlocks = s.deadlocks;
    db.temp_files = s.temp_files;
    db.temp_bytes_mb = s.temp_bytes_mb;
    db.rollbacks = s.rollbacks;
    db.idle_in_transaction_count = s.idle_in_transaction_count;
    db.longest_query_duration_seconds = s.longest_query_duration_seconds;
    db.waiting_for_lock_count = s.waiting_for_lock_count;
    db.posts_dead_tuples = s.posts_dead_tuples;
    db.posts_last_autovacuum = s.posts_last_autovacuum;
}

/// Port of `detectSAMLProviderType` (platform/support_packet.go:600): the first pattern of
/// eleven that the lower-cased URL contains, most specific first.
pub fn detect_saml_provider_type(idp_descriptor_url: &str) -> &'static str {
    if idp_descriptor_url.is_empty() {
        return UNKNOWN_DATA_POINT;
    }
    let url = mm_model::utils::go_to_lower(idp_descriptor_url);
    let has = |needle: &str| url.contains(needle);
    if has("login.microsoftonline.com") || has("sts.windows.net") {
        "Azure AD"
    } else if has(".okta.com") || has(".oktapreview.com") {
        "Okta"
    } else if has(".auth0.com") {
        "Auth0"
    } else if has(".onelogin.com") {
        "OneLogin"
    } else if has("accounts.google.com") {
        "Google Workspace"
    } else if has("sso.jumpcloud.com") {
        "JumpCloud"
    } else if has("duo.com/saml2") {
        "Duo"
    } else if has(".centrify.com") {
        "Centrify"
    } else if has("/realms/") {
        "Keycloak"
    } else if has("/adfs") || has("/federationmetadata/") {
        "ADFS"
    } else if has("shibboleth.net") || has("/idp/shibboleth") {
        "Shibboleth"
    } else {
        UNKNOWN_DATA_POINT
    }
}

/// Port of `utils.GetHostnameFromSiteURL` (channels/utils/utils.go:116).
fn hostname_from_site_url(site_url: &str) -> String {
    mm_model::go_url::go_parse(site_url)
        .map(|u| String::from_utf8_lossy(&u.hostname()).into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------------

/// The three `SSOSettings` fields `probeOAuthProvider` reads.
struct SsoProbe {
    enable: bool,
    discovery_endpoint: String,
    token_endpoint: String,
}

fn sso_of(settings: &mm_model::config::SSOSettings) -> SsoProbe {
    SsoProbe {
        enable: settings.enable.unwrap_or(false),
        discovery_endpoint: settings.discovery_endpoint.clone().unwrap_or_default(),
        token_endpoint: settings.token_endpoint.clone().unwrap_or_default(),
    }
}

fn probe_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|err| err.to_string())
}

/// `http.DefaultClient.Do`'s error as Go prints it: `Get "<url>": <cause>`. The cause is the
/// HTTP client's words, not Go's, except for the timeout, which Go reports as the context's.
fn go_get_error(url: &str, err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return format!(
            "Get {}: context deadline exceeded",
            mm_model::utils::go_quote(url)
        );
    }
    let mut cause: &dyn std::error::Error = err;
    while let Some(next) = cause.source() {
        cause = next;
    }
    format!("Get {}: {cause}", mm_model::utils::go_quote(url))
}

/// Port of `probeOAuthProvider` (platform/support_packet.go:430): disabled, or the discovery
/// document fetched and checked for an `issuer`, or — with no discovery endpoint — any answer at
/// all from the token endpoint, or `fail` when neither is configured.
async fn probe_oauth_provider(sso: SsoProbe) -> mm_model::support_packet::OAuthProviderStatus {
    use mm_model::support_packet::OAuthProviderStatus;
    let status = |status: &str, error: String| OAuthProviderStatus {
        status: status.to_owned(),
        error,
    };
    if !sso.enable {
        return status(STATUS_DISABLED, String::new());
    }
    if !sso.discovery_endpoint.is_empty() {
        return match probe_oidc_discovery(&sso.discovery_endpoint).await {
            Ok(()) => status(STATUS_OK, String::new()),
            Err(err) => status(STATUS_FAIL, err),
        };
    }
    if !sso.token_endpoint.is_empty() {
        return match probe_http_get(&sso.token_endpoint).await {
            Ok(_) => status(STATUS_OK, String::new()),
            Err(err) => status(STATUS_FAIL, err),
        };
    }
    status(
        STATUS_FAIL,
        "no discovery or token endpoint configured".to_owned(),
    )
}

/// A GET that succeeds on any answer: the status and at most a mebibyte of the body.
async fn probe_http_get(url: &str) -> Result<(u16, Vec<u8>), String> {
    let client = probe_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| go_get_error(url, &err))?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let mut response = response;
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = PROBE_BODY_LIMIT.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= PROBE_BODY_LIMIT {
            break;
        }
    }
    Ok((status, body))
}

/// Port of `probeOIDCDiscovery` (platform/support_packet.go:456).
async fn probe_oidc_discovery(url: &str) -> Result<(), String> {
    let (status, body) = probe_http_get(url).await?;
    if status >= 400 {
        return Err(format!(
            "discovery endpoint returned unexpected status {status}"
        ));
    }
    let doc: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|err| format!("discovery endpoint did not return valid JSON: {err}"))?;
    let issuer = doc
        .as_object()
        .and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case("issuer")))
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("");
    if issuer.is_empty() {
        return Err("discovery endpoint response missing required 'issuer' field".to_owned());
    }
    Ok(())
}

/// Port of `testPushProxyConnection` (platform/support_packet.go:509): `GET <server>/version`,
/// where the path is `url.JoinPath`'s.
async fn test_push_proxy_connection(server_url: &str) -> Result<(), String> {
    let mut url = mm_model::go_url::go_parse(server_url).map_err(|err| err.to_string())?;
    let escaped = String::from_utf8_lossy(&url.escaped_path()).into_owned();
    let joined = mm_model::go_path::join(&[&escaped, "version"]);
    url.path = joined.into_bytes();
    url.raw_path = Vec::new();
    let version_url = url.to_go_string();
    let (status, _) = probe_http_get(&version_url).await?;
    if status >= 400 {
        return Err(format!("push proxy returned unexpected status {status}"));
    }
    Ok(())
}

/// The part of `mail.SMTPConfig` `TestConnection` reads on a plain connection with no auth —
/// the only one this port answers ([`SupportPacketForward::SmtpProbe`] sends the rest to Go).
struct SmtpProbe {
    hostname: String,
    server: String,
    port: String,
    timeout_secs: i64,
}

impl SmtpProbe {
    /// Port of `mail.TestConnection` (platform/shared/mail/mail.go:208) over plain TCP:
    /// dial, read the `220` greeting, and — when the site URL has a host — `EHLO`, falling back
    /// to `HELO`. Go then closes the connection **before** `Quit`, so no `QUIT` is ever sent.
    async fn test_connection(&self) -> Result<(), String> {
        use tokio::io::AsyncWriteExt as _;

        let timeout = Duration::from_secs(u64::try_from(self.timeout_secs).unwrap_or(0));
        let address = format!("{}:{}", self.server, self.port);
        let connect = dial(address.clone());
        let stream = if timeout.is_zero() {
            connect.await
        } else {
            tokio::time::timeout(timeout, connect)
                .await
                .unwrap_or_else(|_| Err(format!("dial tcp {address}: i/o timeout")))
        }
        .map_err(|err| format!("unable to connect: unable to connect to the SMTP server: {err}"))?;

        let mut conn = tokio::io::BufReader::new(stream);
        let handshake = async {
            read_smtp_response(&mut conn, 220)
                .await
                .map_err(|err| format!("unable to connect to the SMTP server: {err}"))?;
            if !self.hostname.is_empty() {
                if self.hostname.contains(['\r', '\n']) {
                    return Err("unable to send hello message: smtp: the local name must not contain CR or LF".to_owned());
                }
                let ehlo = smtp_command(&mut conn, &format!("EHLO {}", self.hostname), 250).await;
                if ehlo.is_err() {
                    smtp_command(&mut conn, &format!("HELO {}", self.hostname), 250)
                        .await
                        .map_err(|err| format!("unable to send hello message: {err}"))?;
                }
            }
            Ok::<(), String>(())
        };
        let outcome = if timeout.is_zero() {
            handshake.await
        } else {
            tokio::time::timeout(timeout, handshake)
                .await
                .unwrap_or_else(|_| {
                    Err(
                        "unable to connect to the SMTP server: context deadline exceeded"
                            .to_owned(),
                    )
                })
        };
        let _ = conn.get_mut().shutdown().await;
        outcome.map_err(|err| format!("unable to connect: {err}"))
    }
}

/// `net.Dialer.Dial("tcp", address)`: every resolved address in turn, IPv4 first — the order
/// Go's own resolver reads `/etc/hosts` in for `localhost` — and, when all fail, the **first**
/// failure, whose text names the resolved address (`dial tcp 127.0.0.1:10025: connect:
/// connection refused`), not the host name that was dialled.
async fn dial(address: String) -> Result<tokio::net::TcpStream, String> {
    let mut targets: Vec<std::net::SocketAddr> = tokio::net::lookup_host(address.as_str())
        .await
        .map_err(|err| format!("dial tcp: lookup {address}: {err}"))?
        .collect();
    targets.sort_by_key(|target| !target.is_ipv4());
    let mut first_error = None;
    for target in targets {
        match tokio::net::TcpStream::connect(target).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                first_error.get_or_insert_with(|| go_dial_error(&target.to_string(), &err));
            }
        }
    }
    Err(first_error.unwrap_or_else(|| format!("dial tcp {address}: no suitable address found")))
}

/// `net.Dialer.Dial`'s `*OpError` text for the failures a probe meets.
fn go_dial_error(address: &str, err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            format!("dial tcp {address}: connect: connection refused")
        }
        _ => format!("dial tcp {address}: {err}"),
    }
}

/// `textproto.Conn.Cmd` then `ReadResponse(expect)`.
async fn smtp_command(
    conn: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    line: &str,
    expect: u16,
) -> Result<String, String> {
    use tokio::io::AsyncWriteExt as _;
    conn.get_mut()
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    read_smtp_response(conn, expect).await
}

/// Port of `textproto.Reader.ReadResponse` (net/textproto/reader.go:237): continuation lines
/// `NNN-…` until `NNN …`, the message lines joined by `\n`, and a code other than `expect` is
/// the `*textproto.Error` `"%03d %s"`.
async fn read_smtp_response(
    conn: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    expect: u16,
) -> Result<String, String> {
    use tokio::io::AsyncBufReadExt as _;
    let mut message = Vec::new();
    let mut code = 0u16;
    loop {
        let mut line = String::new();
        let n = conn
            .read_line(&mut line)
            .await
            .map_err(|err| err.to_string())?;
        if n == 0 {
            return Err("EOF".to_owned());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.len() < 4 || !matches!(line.as_bytes()[3], b' ' | b'-') {
            if line.len() == 3 && line.bytes().all(|b| b.is_ascii_digit()) {
                code = line.parse().unwrap_or(0);
                break;
            }
            return Err(format!("short response: {line}"));
        }
        let this: u16 = line[..3]
            .parse()
            .map_err(|_| format!("invalid response code: {line}"))?;
        if code != 0 && this != code {
            return Err(format!("wrong code: {line}"));
        }
        code = this;
        message.push(line[4..].to_owned());
        if line.as_bytes()[3] == b' ' {
            break;
        }
    }
    let message = message.join("\n");
    if code != expect {
        return Err(format!("{code:03} {message}"));
    }
    Ok(message)
}

// ---------------------------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------------------------

/// The Linux readings of `platform/{memory,fd,uptime,container_limits,disk}_linux.go`.
mod machine {
    use std::time::Duration;

    /// `runtime.GOARCH` for this build's target.
    pub fn go_arch() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            "arm" => "arm",
            "powerpc64" => "ppc64le",
            "s390x" => "s390x",
            "riscv64" => "riscv64",
            other => other,
        }
    }

    /// `getTotalMemory` (memory_linux.go:13): `sysinfo(2)`'s `totalram * mem_unit`.
    pub fn total_memory() -> Result<u64, String> {
        // SAFETY: `sysinfo` writes into the zeroed struct it is handed and nothing else.
        let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, exclusively borrowed `sysinfo` for the call's duration.
        let rc = unsafe { libc::sysinfo(&mut info) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // `c_ulong` is `u64` here and `u32` on a 32-bit target, so the conversion is not
        // useless everywhere.
        #[allow(clippy::useless_conversion)]
        let total = u64::from(info.totalram);
        Ok(total.saturating_mul(u64::from(info.mem_unit)))
    }

    /// `getContainerLimits` (container_limits_linux.go:34): cgroup v2's `memory.max` in MB,
    /// rounded up, and `cpu.max`'s quota over its period. A missing file is "no limit".
    pub fn container_limits() -> Result<(f64, u64), String> {
        let read = |path: &str| -> Result<Option<String>, String> {
            match std::fs::read_to_string(path) {
                Ok(s) => Ok(Some(s.trim().to_owned())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.to_string()),
            }
        };
        let mut memory_mb = 0u64;
        let Some(memory) = read("/sys/fs/cgroup/memory.max")? else {
            return Ok((0.0, 0));
        };
        if memory != "max" {
            let bytes: u64 = memory
                .parse()
                .map_err(|err: std::num::ParseIntError| err.to_string())?;
            memory_mb = bytes.div_ceil(1024 * 1024);
        }
        let Some(cpu) = read("/sys/fs/cgroup/cpu.max")? else {
            return Ok((0.0, memory_mb));
        };
        let parts: Vec<&str> = cpu.split_whitespace().collect();
        if parts.len() != 2 {
            return Err("unexpected format in cpu.max".to_owned());
        }
        let mut cpu_limit = 0.0;
        if parts[0] != "max" {
            let quota: f64 = parts[0]
                .parse()
                .map_err(|err: std::num::ParseFloatError| err.to_string())?;
            let period: f64 = parts[1]
                .parse()
                .map_err(|err: std::num::ParseFloatError| err.to_string())?;
            if period > 0.0 {
                cpu_limit = quota / period;
            }
        }
        Ok((cpu_limit, memory_mb))
    }

    /// `os.Hostname`: `/proc/sys/kernel/hostname`, trimmed of its newline.
    pub fn hostname() -> Result<String, String> {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim_end_matches('\n').to_owned())
            .map_err(|err| err.to_string())
    }

    /// `getHostUptimeSeconds` (uptime_linux.go:17): the first field of `/proc/uptime`,
    /// truncated to whole seconds.
    pub fn host_uptime_seconds() -> Result<i64, String> {
        let data = std::fs::read_to_string("/proc/uptime")
            .map_err(|err| format!("failed to read /proc/uptime: {err}"))?;
        let first = data
            .split_whitespace()
            .next()
            .ok_or_else(|| "unexpected /proc/uptime format".to_owned())?;
        let seconds: f64 = first
            .parse()
            .map_err(|err| format!("failed to parse /proc/uptime value: {err}"))?;
        Ok(seconds as i64)
    }

    /// `ps.startTime`, which Go takes when the platform service is built. Here it is the
    /// kernel's record of when this process started — `/proc/self/stat`'s start time after boot,
    /// in clock ticks, over `/proc/stat`'s boot time — so it needs no hook in start-up.
    pub fn process_started_at() -> chrono::DateTime<chrono::Utc> {
        let started = || -> Option<chrono::DateTime<chrono::Utc>> {
            let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
            // The command name is parenthesised and may hold spaces; fields restart after it.
            let after = stat.rsplit_once(')')?.1;
            let ticks: i64 = after.split_whitespace().nth(19)?.parse().ok()?;
            let boot: i64 = std::fs::read_to_string("/proc/stat")
                .ok()?
                .lines()
                .find_map(|l| l.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            // SAFETY: `sysconf` reads a configuration value and has no memory effects.
            let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if hz <= 0 {
                return None;
            }
            let millis = boot * 1000 + ticks * 1000 / hz;
            chrono::DateTime::from_timestamp_millis(millis)
        };
        started().unwrap_or_else(chrono::Utc::now)
    }

    /// `getOpenFileDescriptors` (fd_linux.go:16): the entries of `/proc/self/fd`, less the one
    /// the listing itself opens.
    pub fn open_file_descriptors() -> Result<i64, String> {
        let entries = std::fs::read_dir("/proc/self/fd").map_err(|err| err.to_string())?;
        let count = entries.count() as i64;
        Ok((count - 1).max(0))
    }

    /// `getMaxFileDescriptors` (fd_linux.go:26): the soft `RLIMIT_NOFILE`.
    pub fn max_file_descriptors() -> Result<i64, String> {
        // SAFETY: `getrlimit` writes into the zeroed struct it is handed and nothing else.
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        // SAFETY: `limit` is a valid, exclusively borrowed `rlimit` for the call's duration.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        i64::try_from(limit.rlim_cur)
            .map_err(|_| format!("rlimit.Cur {} overflows int64", limit.rlim_cur))
    }

    /// `diskInfo` (disk_linux.go:17).
    pub struct DiskInfo {
        pub total_mb: u64,
        pub available_mb: u64,
        pub filesystem_type: String,
    }

    /// `getDiskInfo` (disk_linux.go:24): `statfs(2)` on the file store directory, given five
    /// seconds.
    pub async fn disk_info(path: &str) -> Result<DiskInfo, String> {
        let owned = path.to_owned();
        let work = tokio::task::spawn_blocking(move || statfs(&owned));
        match tokio::time::timeout(Duration::from_secs(5), work).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("timed out getting disk space info".to_owned()),
        }
    }

    fn statfs(path: &str) -> Result<DiskInfo, String> {
        let c_path = std::ffi::CString::new(path).map_err(|err| err.to_string())?;
        // SAFETY: `statfs` writes into the zeroed struct it is handed and nothing else.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` a valid out-parameter.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return Err(match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
                _ => std::io::Error::last_os_error().to_string(),
            });
        }
        let bsize = stat.f_bsize as u64;
        Ok(DiskInfo {
            total_mb: (stat.f_blocks as u64).saturating_mul(bsize) / (1024 * 1024),
            available_mb: (stat.f_bavail as u64).saturating_mul(bsize) / (1024 * 1024),
            filesystem_type: fs_type_to_string(stat.f_type as i64).to_owned(),
        })
    }

    /// `fsTypeToString` (disk_linux.go:49), the eight magic numbers `x/sys/unix` names.
    pub fn fs_type_to_string(fs_type: i64) -> &'static str {
        match fs_type {
            0xef53 => "ext4",
            0x5846_5342 => "xfs",
            0x6969 => "nfs",
            0x9123_683e => "btrfs",
            0x0102_1994 => "tmpfs",
            0x517b => "smb",
            0x6573_5546 => "fuse",
            0x794c_7630 => "overlay",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The zip
// ---------------------------------------------------------------------------------------------

/// Port of `App.WriteZipFile` (app/file.go:1433) — `archive/zip` with each entry `Deflate`d and
/// stamped `time.Now()` — in the layout Go's writer produces: a local header with the sizes
/// deferred to a data descriptor (flag bit 3), the extended-timestamp extra field Go adds for a
/// `Modified` time, version 2.0 throughout, then the central directory and its end record.
///
/// The compressed bytes are flate2's, not `compress/flate`'s, so two servers' archives never
/// match byte for byte even with the same clock; what matches is every entry's name, order and
/// content.
pub fn write_zip(
    files: &[FileData],
    now: chrono::DateTime<chrono::Local>,
) -> std::io::Result<Vec<u8>> {
    use chrono::{Datelike as _, Timelike as _};
    let dos_time = ((now.hour() << 11) | (now.minute() << 5) | (now.second() / 2)) as u16;
    let dos_date =
        ((((now.year() - 1980).max(0) as u32) << 9) | (now.month() << 5) | now.day()) as u16;
    let unix = u32::try_from(now.timestamp()).unwrap_or(0);
    // `extTimeExtraID`, five bytes: the flags (modification time present) and the time.
    let mut extra = Vec::with_capacity(9);
    extra.extend_from_slice(&0x5455u16.to_le_bytes());
    extra.extend_from_slice(&5u16.to_le_bytes());
    extra.push(1);
    extra.extend_from_slice(&unix.to_le_bytes());

    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in files {
        let name = entry.filename.as_bytes();
        // Go sets the UTF-8 flag only for a name that needs it.
        let flags: u16 = if name.is_ascii() { 0x0008 } else { 0x0808 };
        let offset = u32::try_from(out.len()).unwrap_or(u32::MAX);
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(5));
        encoder.write_all(&entry.body)?;
        let compressed = encoder.finish()?;
        let mut crc = flate2::Crc::new();
        crc.update(&entry.body);
        let crc = crc.sum();
        let compressed_len = u32::try_from(compressed.len()).unwrap_or(u32::MAX);
        let body_len = u32::try_from(entry.body.len()).unwrap_or(u32::MAX);
        let name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);

        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&dos_time.to_le_bytes());
        out.extend_from_slice(&dos_date.to_le_bytes());
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&extra);
        out.extend_from_slice(&compressed);
        out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&compressed_len.to_le_bytes());
        out.extend_from_slice(&body_len.to_le_bytes());

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&8u16.to_le_bytes());
        central.extend_from_slice(&dos_time.to_le_bytes());
        central.extend_from_slice(&dos_date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&compressed_len.to_le_bytes());
        central.extend_from_slice(&body_len.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        // Comment length, disk number, internal attributes; then the external attributes.
        central.extend_from_slice(&[0; 6]);
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
        central.extend_from_slice(&extra);
    }
    let central_offset = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let central_len = u32::try_from(central.len()).unwrap_or(u32::MAX);
    let count = u16::try_from(files.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&central_len.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// The entries of an archive [`write_zip`] wrote, in order — for the tests, which have no zip
/// reader of their own to trust.
#[cfg(test)]
fn read_zip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read as _;
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    let u32_at = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };
    let end = bytes.len() - 22;
    assert_eq!(u32_at(end), 0x0605_4b50, "end of central directory");
    let count = u16_at(end + 10);
    let mut at = u32_at(end + 16);
    let mut entries = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(at), 0x0201_4b50);
        let crc = u32_at(at + 16) as u32;
        let compressed = u32_at(at + 20);
        let name_len = u16_at(at + 28);
        let extra_len = u16_at(at + 30);
        let local = u32_at(at + 42);
        let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap();
        let data_at = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let mut body = Vec::new();
        flate2::read::DeflateDecoder::new(&bytes[data_at..data_at + compressed])
            .read_to_end(&mut body)
            .unwrap();
        let mut check = flate2::Crc::new();
        check.update(&body);
        assert_eq!(check.sum(), crc, "{name}");
        entries.push((name, body));
        at += 46 + name_len + extra_len;
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multierror_text_matches_go() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_goyaml.json"))
                .expect("generated by reference/dump");
        let m = &corpus["multierror"];
        assert_eq!(multierror_text(&["first".into()]), m["one"]);
        assert_eq!(
            multierror_text(&["first".into(), "second".into()]),
            m["two"]
        );
        assert_eq!(
            multierror_text(&[
                "third\nwith newline".into(),
                "first".into(),
                "second".into()
            ]),
            m["nested"]
        );
        assert_eq!(multierror_text(&["second".into()]), m["restart"]);
    }

    #[test]
    fn the_zip_round_trips_in_order() {
        let files = vec![
            file("metadata.yaml", "version: 1\n"),
            file("empty", Vec::new()),
            file("warning.txt", "1 error occurred:\n\t* x\n\n"),
            file("ünïcode/name.txt", vec![0u8, 1, 2, 255]),
        ];
        let bytes = write_zip(&files, chrono::Local::now()).unwrap();
        let back = read_zip(&bytes);
        let want: Vec<(String, Vec<u8>)> =
            files.into_iter().map(|f| (f.filename, f.body)).collect();
        assert_eq!(back, want);
    }

    #[test]
    fn the_file_name_is_go_s() {
        use chrono::TimeZone as _;
        let now = chrono::FixedOffset::east_opt(19800)
            .unwrap()
            .with_ymd_and_hms(2026, 9, 3, 7, 5, 59)
            .unwrap();
        assert_eq!(
            support_packet_file_name(&now, "Acme Corp."),
            "mm_support_packet_Acme_Corp_2026-09-03T07-05.zip"
        );
        assert_eq!(
            support_packet_file_name(&now, ""),
            "mm_support_packet__2026-09-03T07-05.zip"
        );
    }

    #[test]
    fn advanced_logging_targets_read_file_targets_only() {
        let doc = serde_json::json!({
            "a": {"type": "file", "options": {"filename": "/tmp/a.log"}},
            "b": {"type": "console", "options": {"out": "stdout"}},
            "c": {"Type": "file", "Options": {"FileName": "/tmp/c.log"}},
            "d": {"type": "file"},
        });
        let targets = advanced_logging_targets(&doc).unwrap();
        assert_eq!(
            targets,
            vec![
                Ok("/tmp/a.log".to_owned()),
                Ok("/tmp/c.log".to_owned()),
                Err("unexpected end of JSON input".to_owned()),
            ]
        );
        for empty in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            assert_eq!(advanced_logging_targets(&empty), Ok(vec![]), "{empty}");
        }
        assert!(advanced_logging_targets(&serde_json::json!("x")).is_err());
    }

    #[test]
    fn forward_decisions() {
        let mut config = ModelConfig::default();
        config.file_settings.driver_name = Some("local".into());
        assert_eq!(App::support_packet_forward(&config), None);
        config.email_settings.send_email_notifications = Some(true);
        config.email_settings.connection_security = Some(String::new());
        assert_eq!(App::support_packet_forward(&config), None);
        config.email_settings.enable_smtp_auth = Some(true);
        assert_eq!(
            App::support_packet_forward(&config),
            Some(SupportPacketForward::SmtpProbe)
        );
        config.email_settings.enable_smtp_auth = Some(false);
        for security in [SMTP_TLS, SMTP_STARTTLS] {
            config.email_settings.connection_security = Some(security.into());
            assert_eq!(
                App::support_packet_forward(&config),
                Some(SupportPacketForward::SmtpProbe),
                "{security}"
            );
        }
        config.email_settings.send_email_notifications = Some(false);
        assert_eq!(App::support_packet_forward(&config), None);
        config.file_settings.driver_name = Some("amazons3".into());
        assert_eq!(
            App::support_packet_forward(&config),
            Some(SupportPacketForward::FileBackend)
        );
    }

    #[test]
    fn fs_types() {
        assert_eq!(machine::fs_type_to_string(0xef53), "ext4");
        assert_eq!(machine::fs_type_to_string(0x794c_7630), "overlay");
        assert_eq!(machine::fs_type_to_string(1), "unknown");
    }

    /// The comment map and the omitempty rules against goccy's own output
    /// (`fixtures/behaviour_support_packet.json`).
    mod go_parity {
        use super::super::*;

        fn corpus() -> serde_json::Value {
            serde_json::from_str(include_str!(
                "../../../fixtures/behaviour_support_packet.json"
            ))
            .expect("generated by reference/dump")
        }

        fn diagnostics_from(input: &serde_json::Value) -> SupportPacketDiagnostics {
            let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_owned();
            let i = |v: &serde_json::Value| v.as_i64().unwrap_or_default();
            let u = |v: &serde_json::Value| v.as_u64().unwrap_or_default();
            let t = |v: &serde_json::Value| {
                v.as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.to_utc())
                    .filter(|t| t.timestamp() != -62_135_596_800)
            };
            let status = |v: &serde_json::Value| mm_model::support_packet::OAuthProviderStatus {
                status: s(&v["Status"]),
                error: s(&v["Error"]),
            };
            let mut d = SupportPacketDiagnostics {
                version: i(&input["Version"]),
                ..Default::default()
            };
            let l = &input["License"];
            d.license.company = s(&l["Company"]);
            d.license.users = i(&l["Users"]);
            d.license.sku_short_name = s(&l["SkuShortName"]);
            d.license.is_trial = l["IsTrial"].as_bool().unwrap_or_default();
            d.license.is_gov_sku = l["IsGovSKU"].as_bool().unwrap_or_default();
            d.license.is_non_production = l["IsNonProduction"].as_bool().unwrap_or_default();
            let v = &input["Server"];
            d.server.os = s(&v["OS"]);
            d.server.architecture = s(&v["Architecture"]);
            d.server.hostname = s(&v["Hostname"]);
            d.server.installation_type = s(&v["InstallationType"]);
            d.server.cpu_cores = i(&v["CPUCores"]);
            d.server.total_memory_mb = u(&v["TotalMemoryMB"]);
            d.server.container_cpu_limit = v["ContainerCPULimit"].as_f64().unwrap_or_default();
            d.server.container_memory_limit_mb = u(&v["ContainerMemoryLimitMB"]);
            d.server.process_id = i(&v["ProcessID"]);
            d.server.started_at = t(&v["StartedAt"]);
            d.server.host_started_at = t(&v["HostStartedAt"]);
            d.server.open_file_descriptors = i(&v["OpenFileDescriptors"]);
            d.server.max_file_descriptors = i(&v["MaxFileDescriptors"]);
            d.server.version = s(&v["Version"]);
            d.server.build_hash = s(&v["BuildHash"]);
            d.server.go_version = s(&v["GoVersion"]);
            d.config.source = s(&input["Config"]["Source"]);
            let db = &input["Database"];
            d.database.type_ = s(&db["Type"]);
            d.database.version = s(&db["Version"]);
            d.database.schema_version = s(&db["SchemaVersion"]);
            d.database.master_connections = i(&db["MasterConnections"]);
            d.database.replica_connections = i(&db["ReplicaConnections"]);
            d.database.search_connections = i(&db["SearchConnections"]);
            d.database.master_connections_in_use = i(&db["MasterConnectionsInUse"]);
            d.database.master_connections_idle = i(&db["MasterConnectionsIdle"]);
            d.database.master_pool_wait_count = i(&db["MasterPoolWaitCount"]);
            d.database.master_pool_wait_duration_ms = i(&db["MasterPoolWaitDurationMs"]);
            d.database.master_connections_closed_max_idle =
                i(&db["MasterConnectionsClosedMaxIdle"]);
            d.database.master_connections_closed_max_lifetime =
                i(&db["MasterConnectionsClosedMaxLifetime"]);
            d.database.replica_connections_in_use = i(&db["ReplicaConnectionsInUse"]);
            d.database.replica_connections_idle = i(&db["ReplicaConnectionsIdle"]);
            d.database.replica_pool_wait_count = i(&db["ReplicaPoolWaitCount"]);
            d.database.replica_pool_wait_duration_ms = i(&db["ReplicaPoolWaitDurationMs"]);
            d.database.replica_connections_closed_max_idle =
                i(&db["ReplicaConnectionsClosedMaxIdle"]);
            d.database.replica_connections_closed_max_lifetime =
                i(&db["ReplicaConnectionsClosedMaxLifetime"]);
            d.database.cache_hit_ratio = db["CacheHitRatio"].as_f64();
            d.database.deadlocks = db["Deadlocks"].as_i64();
            d.database.temp_files = db["TempFiles"].as_i64();
            d.database.temp_bytes_mb = db["TempBytesMB"].as_f64();
            d.database.rollbacks = db["Rollbacks"].as_i64();
            d.database.idle_in_transaction_count = db["IdleInTransactionCount"].as_i64();
            d.database.longest_query_duration_seconds = db["LongestQueryDurationSeconds"].as_f64();
            d.database.waiting_for_lock_count = db["WaitingForLockCount"].as_i64();
            d.database.posts_dead_tuples = db["PostsDeadTuples"].as_i64();
            d.database.posts_last_autovacuum = t(&db["PostsLastAutovacuum"]);
            let f = &input["FileStore"];
            d.file_store.status = s(&f["Status"]);
            d.file_store.error = s(&f["Error"]);
            d.file_store.driver = s(&f["Driver"]);
            d.file_store.filesystem_type = s(&f["FilesystemType"]);
            d.file_store.total_mb = u(&f["TotalMB"]);
            d.file_store.available_mb = u(&f["AvailableMB"]);
            d.websocket.connections = i(&input["Websocket"]["Connections"]);
            d.cluster.id = s(&input["Cluster"]["ID"]);
            d.cluster.number_of_nodes = i(&input["Cluster"]["NumberOfNodes"]);
            let n = &input["Notifications"];
            d.notifications.email.status = s(&n["Email"]["Status"]);
            d.notifications.email.error = s(&n["Email"]["Error"]);
            d.notifications.push.status = s(&n["Push"]["Status"]);
            d.notifications.push.error = s(&n["Push"]["Error"]);
            let ld = &input["LDAP"];
            d.ldap.status = s(&ld["Status"]);
            d.ldap.error = s(&ld["Error"]);
            d.ldap.server_name = s(&ld["ServerName"]);
            d.ldap.server_version = s(&ld["ServerVersion"]);
            let sa = &input["SAML"];
            d.saml.provider_type = s(&sa["ProviderType"]);
            d.saml.status = s(&sa["Status"]);
            d.saml.error = s(&sa["Error"]);
            let es = &input["ElasticSearch"];
            d.elastic_search.status = s(&es["Status"]);
            d.elastic_search.backend = s(&es["Backend"]);
            d.elastic_search.server_version = s(&es["ServerVersion"]);
            d.elastic_search.server_plugins = es["ServerPlugins"]
                .as_array()
                .map(|a| a.iter().map(s).collect())
                .unwrap_or_default();
            d.elastic_search.error = s(&es["Error"]);
            let o = &input["OAuthProviders"];
            d.oauth_providers.gitlab = status(&o["GitLab"]);
            d.oauth_providers.google = status(&o["Google"]);
            d.oauth_providers.office365 = status(&o["Office365"]);
            d.oauth_providers.openid = status(&o["OpenID"]);
            d
        }

        #[test]
        fn diagnostics_match_goccy() {
            for case in corpus()["diagnostics"].as_array().expect("cases") {
                let name = case["name"].as_str().expect("name");
                let d = diagnostics_from(&case["input"]);
                let got = diagnostics_yaml(&d);
                let want = case["yaml"].as_str().expect("yaml");
                assert!(got == want, "{name}:\n--- got\n{got}\n--- Go\n{want}");
            }
        }

        #[test]
        fn saml_provider_type_matches_go() {
            for case in corpus()["saml_provider_type"].as_array().expect("cases") {
                let input = case["input"].as_str().expect("input");
                assert_eq!(
                    Some(detect_saml_provider_type(input)),
                    case["output"].as_str(),
                    "{input:?}"
                );
            }
        }
    }
}
