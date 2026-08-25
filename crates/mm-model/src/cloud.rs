//! Port of `model/cloud.go` — the Customer Web Service (CWS) types: products, subscriptions,
//! invoices and the webhook payload.
//!
//! # Four booleans are strings
//!
//! `Subscription.IsFreeTrial`, `ComplianceBlocked` and `WillRenew` are Go `string`s carrying
//! `"true"`/`"false"`, not `bool`s — CWS's own encoding, preserved verbatim. Typing them as
//! `bool` here would reject the payloads CWS actually sends.
//!
//! # `DaysToExpiration` honours a simulated clock
//!
//! In the `test` service environment a subscription may carry `SimulatedCurrentTimeMs`, and the
//! calculation uses it instead of the real clock. That branch is **environment-gated**, so the
//! same subscription answers differently on a production server — see
//! [`Subscription::days_to_expiration`].

use serde::{Deserialize, Serialize};

use crate::ip_filtering::AllowedIPRanges;
use crate::serde_helpers::{is_empty_str, is_none, is_none_or_empty_map};
use crate::service_environment::{SERVICE_ENVIRONMENT_TEST, get_service_environment};
use crate::utils::{StringInterface, get_millis};

pub const EVENT_TYPE_FAILED_PAYMENT: &str = "failed-payment";
pub const EVENT_TYPE_FAILED_PAYMENT_NO_CARD: &str = "failed-payment-no-card";
pub const EVENT_TYPE_SEND_ADMIN_WELCOME_EMAIL: &str = "send-admin-welcome-email";
pub const EVENT_TYPE_SEND_UPGRADE_CONFIRMATION_EMAIL: &str = "send-upgrade-confirmation-email";
pub const EVENT_TYPE_SUBSCRIPTION_CHANGED: &str = "subscription-changed";
pub const EVENT_TYPE_TRIGGER_DELINQUENCY_EMAIL: &str = "trigger-delinquency-email";

/// Port of `model.UpcomingInvoice` (cloud.go:21) — the sentinel invoice id for the next,
/// not-yet-issued invoice.
pub const UPCOMING_INVOICE: &str = "upcoming";

pub const BILLING_SCHEME_PER_SEAT: &str = "per_seat";
pub const BILLING_SCHEME_FLAT_FEE: &str = "flat_fee";
pub const BILLING_SCHEME_SALES_SERVE: &str = "sales_serve";

pub const BILLING_TYPE_LICENSED: &str = "licensed";
pub const BILLING_TYPE_INTERNAL: &str = "internal";

/// The values are the **singular** `year` / `month`, not `yearly` / `monthly`.
pub const RECURRING_INTERVAL_YEARLY: &str = "year";
pub const RECURRING_INTERVAL_MONTHLY: &str = "month";

pub const SUBSCRIPTION_FAMILY_CLOUD: &str = "cloud";
pub const SUBSCRIPTION_FAMILY_ON_PREM: &str = "on-prem";

pub const SKU_STARTER_GOV: &str = "starter-gov";
pub const SKU_PROFESSIONAL_GOV: &str = "professional-gov";
pub const SKU_ENTERPRISE_GOV: &str = "enterprise-gov";
pub const SKU_STARTER: &str = "starter";
pub const SKU_PROFESSIONAL: &str = "professional";
pub const SKU_ENTERPRISE: &str = "enterprise";
pub const SKU_CLOUD_STARTER: &str = "cloud-starter";
pub const SKU_CLOUD_PROFESSIONAL: &str = "cloud-professional";
pub const SKU_CLOUD_ENTERPRISE: &str = "cloud-enterprise";

/// Port of `model.DelinquencyEmail` (cloud.go:262) — the values are **day counts as strings**.
pub const DELINQUENCY_EMAIL_7: &str = "7";
pub const DELINQUENCY_EMAIL_14: &str = "14";
pub const DELINQUENCY_EMAIL_30: &str = "30";
pub const DELINQUENCY_EMAIL_45: &str = "45";
pub const DELINQUENCY_EMAIL_60: &str = "60";
pub const DELINQUENCY_EMAIL_75: &str = "75";
pub const DELINQUENCY_EMAIL_90: &str = "90";

/// Port of `model.Product` (cloud.go:70).
///
/// **`Family` is tagged `product_family`**, not `family`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Product {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "description")]
    pub description: String,

    /// Currency units, fractional — a `float64`, unlike the invoice totals, which are integer
    /// cents.
    #[serde(rename = "price_per_seat")]
    pub price_per_seat: f64,

    #[serde(rename = "add_ons")]
    pub add_ons: Option<Vec<AddOn>>,

    #[serde(rename = "sku")]
    pub sku: String,

    #[serde(rename = "price_id")]
    pub price_id: String,

    #[serde(rename = "product_family")]
    pub family: String,

    #[serde(rename = "recurring_interval")]
    pub recurring_interval: String,

    #[serde(rename = "billing_scheme")]
    pub billing_scheme: String,

    /// The product id this one upgrades to.
    #[serde(rename = "cross_sells_to")]
    pub cross_sells_to: String,
}

impl Product {
    /// Port of `(*Product).IsYearly` (cloud.go:360).
    pub fn is_yearly(&self) -> bool {
        self.recurring_interval == RECURRING_INTERVAL_YEARLY
    }

    /// Port of `(*Product).IsMonthly` (cloud.go:364).
    pub fn is_monthly(&self) -> bool {
        self.recurring_interval == RECURRING_INTERVAL_MONTHLY
    }
}

/// Port of `model.UserFacingProduct` (cloud.go:84) — the subset shown to end users: no
/// description, no add-ons, no price id, no billing scheme.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserFacingProduct {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "sku")]
    pub sku: String,

    #[serde(rename = "price_per_seat")]
    pub price_per_seat: f64,

    #[serde(rename = "recurring_interval")]
    pub recurring_interval: String,

    #[serde(rename = "cross_sells_to")]
    pub cross_sells_to: String,
}

/// Port of `model.AddOn` (cloud.go:94).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddOn {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "price_per_seat")]
    pub price_per_seat: f64,
}

/// Port of `model.StripeSetupIntent` (cloud.go:102) — Stripe's own model, passed through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripeSetupIntent {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "client_secret")]
    pub client_secret: String,
}

/// Port of `model.ConfirmPaymentMethodRequest` (cloud.go:108).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfirmPaymentMethodRequest {
    #[serde(rename = "stripe_setup_intent_id")]
    pub stripe_setup_intent_id: String,

    #[serde(rename = "subscription_id")]
    pub subscription_id: String,
}

/// Port of `model.CloudCustomerInfo` (cloud.go:141) — the editable half of a customer.
///
/// **`CloudAltPaymentMethod` is tagged `monthly_subscription_alt_payment_method`** — the longest
/// name-to-tag mismatch in the package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudCustomerInfo {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "email", skip_serializing_if = "is_empty_str")]
    pub email: String,

    #[serde(rename = "contact_first_name", skip_serializing_if = "is_empty_str")]
    pub contact_first_name: String,

    #[serde(rename = "contact_last_name", skip_serializing_if = "is_empty_str")]
    pub contact_last_name: String,

    #[serde(rename = "num_employees")]
    pub num_employees: i64,

    #[serde(rename = "monthly_subscription_alt_payment_method")]
    pub cloud_alt_payment_method: String,
}

/// Port of `model.CloudCustomer` (cloud.go:113) — the info block **inlined**, plus the ids and
/// addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudCustomer {
    #[serde(flatten)]
    pub info: CloudCustomerInfo,

    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "billing_address")]
    pub billing_address: Option<Address>,

    #[serde(rename = "company_address")]
    pub company_address: Option<Address>,

    #[serde(rename = "payment_method")]
    pub payment_method: Option<PaymentMethod>,
}

/// Port of `model.StartCloudTrialRequest` (cloud.go:122).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartCloudTrialRequest {
    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "subscription_id")]
    pub subscription_id: String,
}

/// Port of `model.ValidateBusinessEmailRequest` (cloud.go:127).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidateBusinessEmailRequest {
    #[serde(rename = "email")]
    pub email: String,
}

/// Port of `model.ValidateBusinessEmailResponse` (cloud.go:131).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidateBusinessEmailResponse {
    #[serde(rename = "is_valid")]
    pub is_valid: bool,
}

/// Port of `model.SubscriptionLicenseSelfServeStatusResponse` (cloud.go:135).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionLicenseSelfServeStatusResponse {
    #[serde(rename = "is_expandable")]
    pub is_expandable: bool,

    #[serde(rename = "is_renewable")]
    pub is_renewable: bool,
}

/// Port of `model.Address` (cloud.go:151).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Address {
    #[serde(rename = "city")]
    pub city: String,

    #[serde(rename = "country")]
    pub country: String,

    #[serde(rename = "line1")]
    pub line1: String,

    #[serde(rename = "line2")]
    pub line2: String,

    #[serde(rename = "postal_code")]
    pub postal_code: String,

    #[serde(rename = "state")]
    pub state: String,
}

/// Port of `model.PaymentMethod` (cloud.go:161).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaymentMethod {
    #[serde(rename = "type")]
    pub type_: String,

    /// The last four digits, as a **string** — leading zeros are significant.
    #[serde(rename = "last_four")]
    pub last_four: String,

    #[serde(rename = "exp_month")]
    pub exp_month: i64,

    #[serde(rename = "exp_year")]
    pub exp_year: i64,

    #[serde(rename = "card_brand")]
    pub card_brand: String,

    #[serde(rename = "name")]
    pub name: String,
}

/// Port of `model.Subscription` (cloud.go:171). See the module docs on the string booleans.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Subscription {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "customer_id")]
    pub customer_id: String,

    #[serde(rename = "product_id")]
    pub product_id: String,

    #[serde(rename = "add_ons")]
    pub add_ons: Option<Vec<String>>,

    #[serde(rename = "start_at")]
    pub start_at: i64,

    #[serde(rename = "end_at")]
    pub end_at: i64,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "seats")]
    pub seats: i64,

    #[serde(rename = "status")]
    pub status: String,

    /// The workspace's hostname; [`Subscription::get_workspace_name_from_dns`] takes its first
    /// label.
    #[serde(rename = "dns")]
    pub dns: String,

    #[serde(rename = "last_invoice")]
    pub last_invoice: Option<Box<Invoice>>,

    #[serde(rename = "upcoming_invoice")]
    pub upcoming_invoice: Option<Box<Invoice>>,

    /// A **string** boolean.
    #[serde(rename = "is_free_trial")]
    pub is_free_trial: String,

    #[serde(rename = "trial_end_at")]
    pub trial_end_at: i64,

    #[serde(rename = "delinquent_since")]
    pub delinquent_since: Option<i64>,

    #[serde(rename = "originally_licensed_seats")]
    pub originally_licensed_seats: i64,

    /// A **string** boolean.
    #[serde(rename = "compliance_blocked")]
    pub compliance_blocked: String,

    #[serde(rename = "billing_type")]
    pub billing_type: String,

    #[serde(rename = "cancel_at")]
    pub cancel_at: Option<i64>,

    /// A **string** boolean.
    #[serde(rename = "will_renew")]
    pub will_renew: String,

    /// Test clocks only — see [`Subscription::days_to_expiration`].
    #[serde(rename = "simulated_current_time_ms")]
    pub simulated_current_time_ms: Option<i64>,

    #[serde(rename = "is_cloud_preview")]
    pub is_cloud_preview: bool,
}

impl Subscription {
    /// Port of `(*Subscription).DaysToExpiration` (cloud.go:196).
    ///
    /// Integer division truncating **toward zero**, so an already-expired subscription reports a
    /// negative, rounded-up day count — the same shape as `License::days_to_expiration`, but
    /// computed in integers rather than through a float.
    ///
    /// The simulated clock is consulted **only** in the `test` service environment; on a
    /// production server a subscription carrying one is ignored.
    pub fn days_to_expiration(&self) -> i64 {
        let mut now = get_millis();
        if get_service_environment() == SERVICE_ENVIRONMENT_TEST {
            if let Some(simulated) = self.simulated_current_time_ms {
                now = simulated;
            }
        }
        (self.end_at - now) / (1000 * 60 * 60 * 24)
    }

    /// Port of `(*Subscription).GetWorkSpaceNameFromDNS` (cloud.go:224).
    ///
    /// The first dot-separated label: `test.mattermost.cloud.com` → `test`. An empty `dns` yields
    /// `""`, matching Go's `strings.Split("", ".")[0]`.
    pub fn get_workspace_name_from_dns(&self) -> &str {
        self.dns.split('.').next().unwrap_or("")
    }
}

/// Port of `model.SubscriptionHistory` (cloud.go:210) — a true-up event on a yearly subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionHistory {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "subscription_id")]
    pub subscription_id: String,

    #[serde(rename = "seats")]
    pub seats: i64,

    #[serde(rename = "create_at")]
    pub create_at: i64,
}

/// Port of `model.SubscriptionHistoryChange` (cloud.go:217) — the same thing without an id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionHistoryChange {
    #[serde(rename = "subscription_id")]
    pub subscription_id: String,

    #[serde(rename = "seats")]
    pub seats: i64,

    #[serde(rename = "create_at")]
    pub create_at: i64,
}

/// Port of `model.Invoice` (cloud.go:229).
///
/// **`Items` is tagged `line_items`**, and the money fields are integer minor units (cents),
/// unlike `Product.PricePerSeat`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Invoice {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "number")]
    pub number: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "total")]
    pub total: i64,

    #[serde(rename = "tax")]
    pub tax: i64,

    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "period_start")]
    pub period_start: i64,

    #[serde(rename = "period_end")]
    pub period_end: i64,

    #[serde(rename = "subscription_id")]
    pub subscription_id: String,

    #[serde(rename = "line_items")]
    pub items: Option<Vec<InvoiceLineItem>>,

    #[serde(rename = "current_product_name")]
    pub current_product_name: String,
}

/// Port of `model.InvoiceLineItem` (cloud.go:244).
///
/// **`Quantity` is a `float64`** while `PricePerUnit` and `Total` are integers — a proration can
/// bill a fractional seat.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InvoiceLineItem {
    #[serde(rename = "price_id")]
    pub price_id: String,

    #[serde(rename = "total")]
    pub total: i64,

    #[serde(rename = "quantity")]
    pub quantity: f64,

    #[serde(rename = "price_per_unit")]
    pub price_per_unit: i64,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "metadata")]
    pub metadata: Option<StringInterface>,

    #[serde(rename = "period_start")]
    pub period_start: i64,

    #[serde(rename = "period_end")]
    pub period_end: i64,
}

/// Port of `model.DelinquencyEmailTrigger` (cloud.go:258).
///
/// **The field is `EmailToTrigger` and the tag is `email_to_send`.**
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelinquencyEmailTrigger {
    #[serde(rename = "email_to_send")]
    pub email_to_trigger: String,
}

/// Port of `model.CWSWebhookPayload` (cloud.go:274) — what the Customer Web Service posts back.
///
/// **`SubscriptionTrialEndUnixTimeStamp` is tagged `trial_end_time_stamp`**, and — unlike every
/// other timestamp here — the field name says Unix **seconds** while the neighbouring
/// `Subscription.TrialEndAt` is milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CWSWebhookPayload {
    /// One of the six `EVENT_TYPE_*` constants.
    #[serde(rename = "event")]
    pub event: String,

    #[serde(rename = "failed_payment")]
    pub failed_payment: Option<FailedPayment>,

    #[serde(rename = "cloud_workspace_owner")]
    pub cloud_workspace_owner: Option<CloudWorkspaceOwner>,

    #[serde(rename = "product_limits")]
    pub product_limits: Option<ProductLimits>,

    #[serde(rename = "subscription")]
    pub subscription: Option<Box<Subscription>>,

    #[serde(rename = "trial_end_time_stamp")]
    pub subscription_trial_end_unix_time_stamp: i64,

    #[serde(rename = "delinquency_email")]
    pub delinquency_email: Option<DelinquencyEmailTrigger>,
}

/// Port of `model.FailedPayment` (cloud.go:283).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FailedPayment {
    #[serde(rename = "card_brand")]
    pub card_brand: String,

    #[serde(rename = "last_four")]
    pub last_four: String,

    #[serde(rename = "failure_message")]
    pub failure_message: String,
}

/// Port of `model.CloudWorkspaceOwner` (cloud.go:290) — the field is `UserName`, the tag is
/// `username`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudWorkspaceOwner {
    #[serde(rename = "username")]
    pub user_name: String,
}

/// Port of `model.SubscriptionChange` (cloud.go:294).
///
/// **`Feedback` is tagged `downgrade_feedback`** — the field is only populated when the change is
/// a downgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionChange {
    #[serde(rename = "product_id")]
    pub product_id: String,

    #[serde(rename = "seats")]
    pub seats: i64,

    #[serde(rename = "downgrade_feedback")]
    pub feedback: Option<Feedback>,

    #[serde(rename = "shipping_address")]
    pub shipping_address: Option<Address>,

    #[serde(rename = "customer")]
    pub customer: Option<CloudCustomerInfo>,
}

/// Port of `model.FilesLimits` (cloud.go:302). `null` means **no limit**, which is why the field
/// is a pointer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesLimits {
    /// Bytes.
    #[serde(rename = "total_storage")]
    pub total_storage: Option<i64>,
}

/// Port of `model.MessagesLimits` (cloud.go:306).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagesLimits {
    /// Posts of history retained.
    #[serde(rename = "history")]
    pub history: Option<i64>,
}

/// Port of `model.TeamsLimits` (cloud.go:310).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamsLimits {
    #[serde(rename = "active")]
    pub active: Option<i64>,
}

/// Port of `model.ProductLimits` (cloud.go:314).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProductLimits {
    #[serde(rename = "files", skip_serializing_if = "is_none")]
    pub files: Option<FilesLimits>,

    #[serde(rename = "messages", skip_serializing_if = "is_none")]
    pub messages: Option<MessagesLimits>,

    #[serde(rename = "teams", skip_serializing_if = "is_none")]
    pub teams: Option<TeamsLimits>,
}

/// Port of `model.CreateSubscriptionRequest` (cloud.go:321).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateSubscriptionRequest {
    #[serde(rename = "product_id")]
    pub product_id: String,

    #[serde(rename = "add_ons")]
    pub add_ons: Option<Vec<String>>,

    #[serde(rename = "seats")]
    pub seats: i64,

    #[serde(rename = "total")]
    pub total: f64,

    #[serde(rename = "internal_purchase_order")]
    pub internal_purchase_order: String,

    #[serde(rename = "discount_id")]
    pub discount_id: String,
}

/// Port of `model.Installation` (cloud.go:330).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Installation {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(rename = "allowed_ip_ranges")]
    pub allowed_ip_ranges: Option<AllowedIPRanges>,
}

/// Port of `model.Feedback` (cloud.go:336).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Feedback {
    #[serde(rename = "reason")]
    pub reason: String,

    #[serde(rename = "comments")]
    pub comments: String,
}

impl Feedback {
    /// Port of `(*Feedback).ToMap` (cloud.go:368).
    ///
    /// Go marshals and unmarshals to get a `map[string]any`, and returns a **nil map** on error —
    /// which cannot happen for two strings. Both keys are always present.
    pub fn to_map(&self) -> StringInterface {
        let mut out = StringInterface::new();
        out.insert(
            "reason".to_string(),
            serde_json::Value::String(self.reason.clone()),
        );
        out.insert(
            "comments".to_string(),
            serde_json::Value::String(self.comments.clone()),
        );
        out
    }
}

/// Port of `model.WorkspaceDeletionRequest` (cloud.go:341) — `Feedback` tagged
/// **`delete_feedback`** here, against `downgrade_feedback` on `SubscriptionChange`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceDeletionRequest {
    #[serde(rename = "subscription_id")]
    pub subscription_id: String,

    #[serde(rename = "delete_feedback")]
    pub feedback: Option<Feedback>,
}

/// Port of `model.MessageDescriptor` (cloud.go:347) — an i18n descriptor as the web app's
/// `react-intl` expects it, which is why **`defaultMessage` is camelCase**.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageDescriptor {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "defaultMessage")]
    pub default_message: String,

    #[serde(rename = "values", skip_serializing_if = "is_none_or_empty_map")]
    pub values: Option<StringInterface>,
}

/// Port of `model.PreviewModalContentData` (cloud.go:354) — fetched from S3, so **every key is
/// camelCase**: `skuLabel`, `videoUrl`, `videoPoster`, `useCase`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewModalContentData {
    #[serde(rename = "skuLabel")]
    pub sku_label: MessageDescriptor,

    #[serde(rename = "title")]
    pub title: MessageDescriptor,

    #[serde(rename = "subtitle")]
    pub subtitle: MessageDescriptor,

    #[serde(rename = "videoUrl")]
    pub video_url: String,

    #[serde(rename = "videoPoster", skip_serializing_if = "is_empty_str")]
    pub video_poster: String,

    #[serde(rename = "useCase")]
    pub use_case: String,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn product_round_trips_the_fixture() {
        assert_fixture_round_trips!(Product, "product");
    }
    #[test]
    fn user_facing_product_round_trips_the_fixture() {
        assert_fixture_round_trips!(UserFacingProduct, "user_facing_product");
    }
    #[test]
    fn add_on_round_trips_the_fixture() {
        assert_fixture_round_trips!(AddOn, "add_on");
    }
    #[test]
    fn stripe_setup_intent_round_trips_the_fixture() {
        assert_fixture_round_trips!(StripeSetupIntent, "stripe_setup_intent");
    }
    #[test]
    fn confirm_payment_method_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ConfirmPaymentMethodRequest,
            "confirm_payment_method_request"
        );
    }
    #[test]
    fn cloud_customer_round_trips_the_fixture() {
        assert_fixture_round_trips!(CloudCustomer, "cloud_customer");
    }
    #[test]
    fn start_cloud_trial_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(StartCloudTrialRequest, "start_cloud_trial_request");
    }
    #[test]
    fn validate_business_email_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ValidateBusinessEmailRequest,
            "validate_business_email_request"
        );
    }
    #[test]
    fn validate_business_email_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ValidateBusinessEmailResponse,
            "validate_business_email_response"
        );
    }
    #[test]
    fn subscription_license_self_serve_status_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            SubscriptionLicenseSelfServeStatusResponse,
            "subscription_license_self_serve_status_response"
        );
    }
    #[test]
    fn cloud_customer_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(CloudCustomerInfo, "cloud_customer_info");
    }
    #[test]
    fn address_round_trips_the_fixture() {
        assert_fixture_round_trips!(Address, "address");
    }
    #[test]
    fn payment_method_round_trips_the_fixture() {
        assert_fixture_round_trips!(PaymentMethod, "payment_method");
    }
    #[test]
    fn subscription_round_trips_the_fixture() {
        assert_fixture_round_trips!(Subscription, "subscription");
    }
    #[test]
    fn subscription_history_round_trips_the_fixture() {
        assert_fixture_round_trips!(SubscriptionHistory, "subscription_history");
    }
    #[test]
    fn subscription_history_change_round_trips_the_fixture() {
        assert_fixture_round_trips!(SubscriptionHistoryChange, "subscription_history_change");
    }
    #[test]
    fn invoice_round_trips_the_fixture() {
        assert_fixture_round_trips!(Invoice, "invoice");
    }
    #[test]
    fn invoice_line_item_round_trips_the_fixture() {
        assert_fixture_round_trips!(InvoiceLineItem, "invoice_line_item");
    }
    #[test]
    fn delinquency_email_trigger_round_trips_the_fixture() {
        assert_fixture_round_trips!(DelinquencyEmailTrigger, "delinquency_email_trigger");
    }
    #[test]
    fn cws_webhook_payload_round_trips_the_fixture() {
        assert_fixture_round_trips!(CWSWebhookPayload, "cws_webhook_payload");
    }
    #[test]
    fn failed_payment_round_trips_the_fixture() {
        assert_fixture_round_trips!(FailedPayment, "failed_payment");
    }
    #[test]
    fn cloud_workspace_owner_round_trips_the_fixture() {
        assert_fixture_round_trips!(CloudWorkspaceOwner, "cloud_workspace_owner");
    }
    #[test]
    fn subscription_change_round_trips_the_fixture() {
        assert_fixture_round_trips!(SubscriptionChange, "subscription_change");
    }
    #[test]
    fn files_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(FilesLimits, "files_limits");
    }
    #[test]
    fn messages_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(MessagesLimits, "messages_limits");
    }
    #[test]
    fn teams_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(TeamsLimits, "teams_limits");
    }
    #[test]
    fn product_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(ProductLimits, "product_limits");
    }
    #[test]
    fn create_subscription_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(CreateSubscriptionRequest, "create_subscription_request");
    }
    #[test]
    fn installation_round_trips_the_fixture() {
        assert_fixture_round_trips!(Installation, "installation");
    }
    #[test]
    fn feedback_round_trips_the_fixture() {
        assert_fixture_round_trips!(Feedback, "feedback");
    }
    #[test]
    fn workspace_deletion_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(WorkspaceDeletionRequest, "workspace_deletion_request");
    }
    #[test]
    fn message_descriptor_round_trips_the_fixture() {
        assert_fixture_round_trips!(MessageDescriptor, "message_descriptor");
    }
    #[test]
    fn preview_modal_content_data_round_trips_the_fixture() {
        assert_fixture_round_trips!(PreviewModalContentData, "preview_modal_content_data");
    }
}
