//! The plugin API's file, dialog and mail methods (app/plugin_api.go:1046-1152), each a thin
//! wrapper over the app function a REST route already uses — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! The trait methods in `crate::plugin_api` delegate here one line each; what each answers, and
//! why, is on the function below.
//!
//! # What is not implemented, per call
//!
//! A shape this server cannot answer is Go's `API <Name> called but not implemented.`, decided
//! before anything is written:
//!
//! - `GetFileInfo`, `GetFile`, `GetFileLink` and `GetFileInfos` when a row is an image with no
//!   stored mini preview — which is **every raster image `UploadFile` makes**, because that path
//!   generates none. Go repairs the row on the read; the repair is [D-891]'s.
//! - `UploadFile` of a `WebP` canvas declaring alpha, which this port does not decode.
//! - A file backend this port does not drive, on any method that reads or writes a file; for
//!   `UploadFile` that is found at the write, after the plugins' `FileWillBeUploaded` ran.
//! - `SendMail` past its three refusals while `SendEmailNotifications` is on: there is no mail
//!   sender here ([D-238]).

use mm_model::file_info::{FileInfo, GetFileInfosOptions};
use mm_model::integration_action::{
    Dialog, DialogActionButton, DialogDateTimeConfig, DialogElement, OpenDialogRequest,
    PostActionOptions,
};
use mm_model::utils::AppError;
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model as wire_model;
use mm_plugin::wire::plugin as api;

use super::AppPluginApi;
use crate::plugin_hooks::{HookContext, file_info_to_wire};
use crate::post::PrepareError;

/// A pointer return and its `*AppError`, as the generated `Z_<Method>Returns` carry them.
type Reply<W> = (Option<Box<W>>, Option<Box<wire_model::AppError>>);

/// A slice gob delivered: an empty one was omitted, so it is Go's nil.
fn nil_if_empty<T>(items: Vec<T>) -> Option<Vec<T>> {
    (!items.is_empty()).then_some(items)
}

/// `model.GetFileInfosOptions` from gob. A nil pointer is the zero options, as the store's
/// `if opt == nil { opt = &model.GetFileInfosOptions{} }` makes it.
pub fn file_infos_options_from_wire(
    wire: Option<Box<wire_model::GetFileInfosOptions>>,
) -> GetFileInfosOptions {
    let Some(wire) = wire else {
        return GetFileInfosOptions::default();
    };
    GetFileInfosOptions {
        user_ids: nil_if_empty(wire.user_ids),
        channel_ids: nil_if_empty(wire.channel_ids),
        since: wire.since,
        include_deleted: wire.include_deleted,
        sort_by: wire.sort_by,
        sort_descending: wire.sort_descending,
        only_empty_content: wire.only_empty_content,
    }
}

/// `model.OpenDialogRequest` from gob — a value, not a pointer, so there is always one.
///
/// Gob cannot tell an empty slice from a nil one, so `Elements` and each element's `Options`
/// arrive nil when the plugin sent them empty, and the `open_dialog` JSON says `null` for both —
/// on Go's host too. The action button's context map is `omitempty` either way.
pub fn open_dialog_request_from_wire(wire: wire_model::OpenDialogRequest) -> OpenDialogRequest {
    let dialog = wire.dialog;
    OpenDialogRequest {
        trigger_id: wire.trigger_id,
        url: wire.url,
        dialog: Dialog {
            callback_id: dialog.callback_id,
            title: dialog.title,
            introduction_text: dialog.introduction_text,
            icon_url: dialog.icon_url,
            elements: nil_if_empty(
                dialog
                    .elements
                    .into_iter()
                    .map(dialog_element_from_wire)
                    .collect(),
            ),
            submit_label: dialog.submit_label,
            notify_on_cancel: dialog.notify_on_cancel,
            state: dialog.state,
            source_url: dialog.source_url,
        },
    }
}

fn dialog_element_from_wire(wire: wire_model::DialogElement) -> DialogElement {
    DialogElement {
        display_name: wire.display_name,
        name: wire.name,
        element_type: wire.r#type,
        sub_type: wire.sub_type,
        default: wire.default,
        placeholder: wire.placeholder,
        help_text: wire.help_text,
        optional: wire.optional,
        min_length: wire.min_length,
        max_length: wire.max_length,
        data_source: wire.data_source,
        data_source_url: wire.data_source_url,
        options: nil_if_empty(
            wire.options
                .into_iter()
                .map(|o| PostActionOptions {
                    text: o.text,
                    value: o.value,
                })
                .collect(),
        ),
        multi_select: wire.multi_select,
        allow_multiple: wire.allow_multiple,
        refresh: wire.refresh,
        date_time_config: wire.date_time_config.map(|c| DialogDateTimeConfig {
            min_date: c.min_date,
            max_date: c.max_date,
            time_interval: c.time_interval,
            location_timezone: c.location_timezone,
            manual_time_entry: c.manual_time_entry,
            allow_manual_time_entry: c.allow_manual_time_entry,
        }),
        min_date: wire.min_date,
        max_date: wire.max_date,
        time_interval: wire.time_interval,
        action_button: wire.action_button.map(|b| DialogActionButton {
            url: b.url,
            context: b.context.into_iter().collect(),
        }),
    }
}

/// `PluginAPI.SendMail`'s three refusals (app/plugin_api.go:1136), in Go's order.
pub fn send_mail_refusal(to: &str, subject: &str, html_body: &str) -> Option<Box<AppError>> {
    let id = if to.is_empty() {
        "plugin_api.send_mail.missing_to"
    } else if subject.is_empty() {
        "plugin_api.send_mail.missing_subject"
    } else if html_body.is_empty() {
        "plugin_api.send_mail.missing_htmlbody"
    } else {
        return None;
    };
    Some(AppError::boxed("SendMail", id, None, String::new(), 400))
}

impl AppPluginApi {
    /// A file read's two returns, with the one shape this server does not reproduce answered as
    /// not implemented.
    fn file_reply<T, W>(
        &self,
        method: &'static str,
        result: Result<T, PrepareError>,
        convert: impl FnOnce(T) -> W,
    ) -> Result<Reply<W>, NotImplemented> {
        match result {
            Ok(value) => Ok((Some(Box::new(convert(value))), None)),
            Err(PrepareError::App(err)) => Ok((None, self.wire(err))),
            Err(PrepareError::Unreproducible(why)) => Err(self.not_implemented(method, why)),
        }
    }

    /// Bytes and their error: a failure is nil bytes.
    fn bytes_reply(
        &self,
        method: &'static str,
        result: Result<Vec<u8>, PrepareError>,
    ) -> Result<(Vec<u8>, Option<Box<wire_model::AppError>>), NotImplemented> {
        match result {
            Ok(bytes) => Ok((bytes, None)),
            Err(PrepareError::App(err)) => Ok((Vec::new(), self.wire(err))),
            Err(PrepareError::Unreproducible(why)) => Err(self.not_implemented(method, why)),
        }
    }

    /// Port of `PluginAPI.UploadFile` (app/plugin_api.go:1087); see [`crate::App::upload_file`].
    /// The upload has no user and no team, and the API's empty context reaches
    /// `FileWillBeUploaded`.
    pub(super) async fn files_upload_file(
        &self,
        args: api::Z_UploadFileArgs,
    ) -> Result<api::Z_UploadFileReturns, NotImplemented> {
        let result = self
            .app
            .upload_file(&HookContext::default(), args.a, &args.b, &args.c)
            .await;
        let (a, b) = self.file_reply("UploadFile", result, |i| file_info_to_wire(&i))?;
        Ok(api::Z_UploadFileReturns { a, b })
    }

    /// Port of `PluginAPI.GetFileInfo` (app/plugin_api.go:1050): the live row, unsanitised —
    /// `Path` and `Content` included, since gob carries every exported field.
    pub(super) async fn files_get_file_info(
        &self,
        args: api::Z_GetFileInfoArgs,
    ) -> Result<api::Z_GetFileInfoReturns, NotImplemented> {
        let result = self.app.get_file_info(&args.a).await;
        let (a, b) = self.file_reply("GetFileInfo", result, |i| file_info_to_wire(&i))?;
        Ok(api::Z_GetFileInfoReturns { a, b })
    }

    /// Port of `PluginAPI.GetFileInfos` (app/plugin_api.go:1058); see
    /// [`crate::App::get_file_infos`].
    pub(super) async fn files_get_file_infos(
        &self,
        args: api::Z_GetFileInfosArgs,
    ) -> Result<api::Z_GetFileInfosReturns, NotImplemented> {
        let options = file_infos_options_from_wire(args.c);
        let answer = match self.app.get_file_infos(args.a, args.b, &options).await {
            Ok(infos) => api::Z_GetFileInfosReturns {
                a: infos.iter().map(file_info_to_wire).collect(),
                b: None,
            },
            Err(PrepareError::App(err)) => api::Z_GetFileInfosReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
            Err(PrepareError::Unreproducible(why)) => {
                return Err(self.not_implemented("GetFileInfos", why));
            }
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetFile` (app/plugin_api.go:1083); see [`crate::App::get_file`].
    pub(super) async fn files_get_file(
        &self,
        args: api::Z_GetFileArgs,
    ) -> Result<api::Z_GetFileReturns, NotImplemented> {
        let (a, b) = self.bytes_reply("GetFile", self.app.get_file(&args.a).await)?;
        Ok(api::Z_GetFileReturns { a, b })
    }

    /// Port of `PluginAPI.ReadFile` (app/plugin_api.go:1079): any path in the file store, as the
    /// backend resolves it — no row is consulted.
    pub(super) async fn files_read_file(
        &self,
        args: api::Z_ReadFileArgs,
    ) -> Result<api::Z_ReadFileReturns, NotImplemented> {
        let (a, b) = self.bytes_reply("ReadFile", self.app.read_file(&args.a).await)?;
        Ok(api::Z_ReadFileReturns { a, b })
    }

    /// Port of `PluginAPI.GetFileLink` (app/plugin_api.go:1062): refused with a **501** while
    /// `FileSettings.EnablePublicLink` is off, before the row is read; then `GetFileInfo`, a 400
    /// for a file no post claims, and the public link under the configured site URL — which is
    /// the configured one, empty or not, not the request's.
    pub(super) async fn files_get_file_link(
        &self,
        args: api::Z_GetFileLinkArgs,
    ) -> Result<api::Z_GetFileLinkReturns, NotImplemented> {
        let refuse = |id: &str, detail: String, status: i32| api::Z_GetFileLinkReturns {
            a: String::new(),
            b: self.wire(AppError::boxed("GetFileLink", id, None, detail, status)),
        };
        let config = self.app.config();
        if !config.enable_public_link {
            return Ok(refuse(
                "plugin_api.get_file_link.disabled.app_error",
                String::new(),
                501,
            ));
        }
        let info: FileInfo = match self.app.get_file_info(&args.a).await {
            Ok(info) => info,
            Err(PrepareError::App(err)) => {
                return Ok(api::Z_GetFileLinkReturns {
                    a: String::new(),
                    b: self.wire(err),
                });
            }
            Err(PrepareError::Unreproducible(why)) => {
                return Err(self.not_implemented("GetFileLink", why));
            }
        };
        if info.post_id.is_empty() {
            return Ok(refuse(
                "plugin_api.get_file_link.no_post.app_error",
                format!("file_id={}", info.id),
                400,
            ));
        }
        let site_url = config.site_url.as_deref().unwrap_or_default();
        Ok(api::Z_GetFileLinkReturns {
            a: self.app.generate_public_link(site_url, &info),
            b: None,
        })
    }

    /// Port of `PluginAPI.CopyFileInfos` (app/plugin_api.go:1046); see
    /// [`crate::App::copy_file_infos`].
    pub(super) async fn files_copy_file_infos(
        &self,
        args: api::Z_CopyFileInfosArgs,
    ) -> Result<api::Z_CopyFileInfosReturns, NotImplemented> {
        let answer = match self.app.copy_file_infos(&args.a, &args.b).await {
            Ok(ids) => api::Z_CopyFileInfosReturns { a: ids, b: None },
            Err(err) => api::Z_CopyFileInfosReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.SetFileSearchableContent` (app/plugin_api.go:1054); see
    /// [`crate::App::set_file_searchable_content`].
    pub(super) async fn files_set_file_searchable_content(
        &self,
        args: api::Z_SetFileSearchableContentArgs,
    ) -> Result<api::Z_SetFileSearchableContentReturns, NotImplemented> {
        let result = self.app.set_file_searchable_content(&args.a, &args.b).await;
        Ok(api::Z_SetFileSearchableContentReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.OpenInteractiveDialog` (app/plugin_api.go:1117); see
    /// [`crate::App::open_interactive_dialog`]: the trigger id is verified against this server's
    /// signing key and its age, an invalid dialog is only logged, and the request goes to the
    /// trigger's user as `open_dialog`.
    pub(super) async fn files_open_interactive_dialog(
        &self,
        args: api::Z_OpenInteractiveDialogArgs,
    ) -> Result<api::Z_OpenInteractiveDialogReturns, NotImplemented> {
        let request = open_dialog_request_from_wire(args.a);
        let result = self.app.open_interactive_dialog(request).await;
        Ok(api::Z_OpenInteractiveDialogReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.SendMail` (app/plugin_api.go:1136): the recipient, the subject and the
    /// body must each be set, in that order; then `SendNotificationMail`, which sends nothing
    /// and succeeds while `EmailSettings.SendEmailNotifications` is off. With it on the mail
    /// would go out, and there is no sender here ([D-238]).
    pub(super) async fn files_send_mail(
        &self,
        args: api::Z_SendMailArgs,
    ) -> Result<api::Z_SendMailReturns, NotImplemented> {
        if let Some(refusal) = send_mail_refusal(&args.a, &args.b, &args.c) {
            return Ok(api::Z_SendMailReturns {
                a: self.wire(refusal),
            });
        }
        if self.app.config().send_email_notifications {
            return Err(self.not_implemented("SendMail", "no SMTP sender is ported (D-238)"));
        }
        Ok(api::Z_SendMailReturns { a: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each refusal needs every field before it set, and names the first one missing.
    #[test]
    fn send_mail_refuses_in_gos_order() {
        let id = |to, subject, body| send_mail_refusal(to, subject, body).map(|e| e.id);
        assert_eq!(
            id("", "", "").as_deref(),
            Some("plugin_api.send_mail.missing_to")
        );
        assert_eq!(
            id("a@b", "", "").as_deref(),
            Some("plugin_api.send_mail.missing_subject")
        );
        assert_eq!(
            id("a@b", "s", "").as_deref(),
            Some("plugin_api.send_mail.missing_htmlbody")
        );
        assert_eq!(id("a@b", "s", "<b>x</b>"), None);
        let refusal = send_mail_refusal("", "s", "b").expect("refused");
        assert_eq!(
            (refusal.status_code, refusal.where_.as_str()),
            (400, "SendMail")
        );
    }

    /// A nil options pointer is the zero options, and an empty list is nil — no filter either way.
    #[test]
    fn file_infos_options_cross_as_the_store_reads_them() {
        assert_eq!(
            file_infos_options_from_wire(None),
            GetFileInfosOptions::default()
        );
        let wire = wire_model::GetFileInfosOptions {
            user_ids: vec![],
            channel_ids: vec!["c".into()],
            since: 5,
            include_deleted: true,
            sort_by: "Size".into(),
            sort_descending: true,
            only_empty_content: true,
        };
        let options = file_infos_options_from_wire(Some(Box::new(wire)));
        assert_eq!(options.user_ids, None);
        assert_eq!(options.channel_ids, Some(vec!["c".to_owned()]));
        assert_eq!(
            (
                options.since,
                options.include_deleted,
                options.sort_by.as_str(),
                options.sort_descending,
                options.only_empty_content
            ),
            (5, true, "Size", true, true)
        );
    }

    /// Gob's empty slices are Go's nil ones, which the `open_dialog` JSON renders as `null`.
    #[test]
    fn an_empty_dialog_list_is_null_in_the_event() {
        let wire = wire_model::OpenDialogRequest {
            trigger_id: "t".into(),
            url: "u".into(),
            dialog: wire_model::Dialog {
                title: "T".into(),
                elements: vec![wire_model::DialogElement {
                    name: "e".into(),
                    r#type: "select".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        let request = open_dialog_request_from_wire(wire);
        let json = serde_json::to_value(&request).expect("json");
        assert_eq!(
            json["dialog"]["elements"][0]["options"],
            serde_json::Value::Null
        );
        assert_eq!(json["dialog"]["elements"][0]["type"], "select");
        let empty = open_dialog_request_from_wire(wire_model::OpenDialogRequest::default());
        assert_eq!(
            serde_json::to_value(&empty).expect("json")["dialog"]["elements"],
            serde_json::Value::Null
        );
    }
}
