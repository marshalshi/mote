//! Read-only Google Sheets sync foundation (slice 1).
//!
//! Implements service-account authentication and fetching of a spreadsheet
//! tab's values, plus a pure normalize/filter function that turns raw rows
//! into [`NormalizedRow`]s, applying the eligibility rules and detecting
//! validation conflicts.
//!
//! # Auth model
//!
//! A Google service account signs a JWT (RS256) with its private key and
//! exchanges it for a short-lived access token at
//! `https://oauth2.googleapis.com/token` (JWT bearer grant). That token is
//! then used for read-only Sheets API calls.
//!
//! # Testability
//!
//! All network calls ([`fetch_access_token`], [`fetch_values`],
//! [`fetch_sheet_values`]) take a `&reqwest::Client` and are never exercised
//! by unit tests — they would hit live Google endpoints. Everything that can
//! be tested without a network is exposed as a pure function:
//! credential parsing, JWT construction/signing, URL building, cell
//! coercion, date parsing, and row normalization.
//!
//! # Wiring status
//!
//! Slice 2 consumes the normalization types ([`NormalizedRow`],
//! [`RowConflict`], [`InvalidRow`], [`NormalizationResult`]) from the PM
//! import store/service. Slice 3 wires the HTTP fetch path into the PM
//! import tools through the [`SheetFetcher`] trait ([`HttpSheetFetcher`]),
//! so tools can be tested with an injected mock instead of live Google HTTP.
//! Dead-code analysis remains suppressed for the few peripheral helpers
//! (token response fields, etc.).

#![allow(dead_code)]

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use url::Url;

/// Read-only OAuth scope for Google Sheets service accounts.
pub const SHEETS_READONLY_SCOPE: &str =
    "https://www.googleapis.com/auth/spreadsheets.readonly";

/// Google OAuth2 token endpoint used for the JWT bearer grant.
pub const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Base URL of the Google Sheets v4 REST API.
pub const SHEETS_API_BASE: &str =
    "https://sheets.googleapis.com/v4/spreadsheets";

/// Column names expected in the sheet, as observed in the `US` tab.
pub const COL_OFFICE: &str = "Office";
pub const COL_ACTIVITY_TYPE: &str = "Type of Activity";
pub const COL_TITLE: &str = "Title of Activity";
pub const COL_SITE: &str = "Site";
pub const COL_START_DATE: &str = "Start Date";
pub const COL_END_DATE: &str = "End Date";
pub const COL_JOB_LEADER: &str = "Job Leader";
pub const COL_TEAM_MEMBER: &str = "Team Member";
pub const COL_ROBOTS: &str = "Robots";
pub const COL_ADDITIONAL_INFO: &str = "Additional Info";
pub const COL_DONE: &str = "Done";

/// Date format of source cells, e.g. `5-Aug-2026` (no zero-padding on day).
const SOURCE_DATE_FORMAT: &str = "%-d-%b-%Y";
/// Normalized date format, e.g. `2026-08-05`.
const NORMALIZED_DATE_FORMAT: &str = "%Y-%m-%d";
/// JWT lifetime for the access-token assertion (Google accepts up to 1 hour).
const JWT_TTL_SECS: u64 = 3600;

// ── Service account credentials ────────────────────────────────────────────

/// A Google service-account credential file (JSON downloaded from the
/// Google Cloud console).
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccount {
    #[serde(rename = "type")]
    pub account_type: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(rename = "private_key_id", default)]
    pub private_key_id: String,
    /// PEM-encoded RSA private key (with literal `\n` escapes in the JSON).
    #[serde(rename = "private_key")]
    pub private_key: String,
    #[serde(rename = "client_email")]
    pub client_email: String,
    #[serde(rename = "client_id", default)]
    pub client_id: String,
    /// Token endpoint; falls back to Google's when absent.
    #[serde(rename = "token_uri", default)]
    pub token_uri: Option<String>,
}

impl ServiceAccount {
    /// Parse a service-account JSON string.
    pub fn from_json_str(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parsing service account JSON")
    }

    /// Load a service-account JSON file.
    pub fn from_json_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!("reading service account credentials: {}", path.display())
        })?;
        Self::from_json_str(&raw)
    }

    /// The token endpoint to use, defaulting to Google's when the credential
    /// file does not specify one.
    pub fn token_uri(&self) -> &str {
        self.token_uri.as_deref().unwrap_or(GOOGLE_TOKEN_URI)
    }
}

// ── JWT bearer grant ───────────────────────────────────────────────────────

/// Build the claims for a service-account JWT assertion.
///
/// `now_unix_secs` is injected so tests can verify exact `iat`/`exp` values.
fn jwt_claims(
    account: &ServiceAccount,
    now_unix_secs: u64,
    ttl_secs: u64,
) -> serde_json::Value {
    serde_json::json!({
        "iss": account.client_email,
        "scope": SHEETS_READONLY_SCOPE,
        "aud": account.token_uri(),
        "iat": now_unix_secs,
        "exp": now_unix_secs + ttl_secs,
    })
}

/// Sign a JWT assertion for the service account using RS256.
///
/// This is pure with respect to its inputs: the only nondeterminism is the
/// RSA PKCS#1 v1.5 signature padding, so the same inputs never produce the
/// same bytes but always produce a valid, decodable assertion.
fn sign_jwt(
    account: &ServiceAccount,
    claims: &serde_json::Value,
) -> Result<String> {
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(
        account.private_key.as_bytes(),
    )
    .context(
        "parsing service account private key (expected PEM-encoded RSA key)",
    )?;
    jsonwebtoken::encode(&header, claims, &key).context("signing JWT assertion")
}

/// Build a signed JWT assertion at an explicit timestamp (test-friendly core).
pub fn build_signed_jwt_at(
    account: &ServiceAccount,
    now_unix_secs: u64,
) -> Result<String> {
    let claims = jwt_claims(account, now_unix_secs, JWT_TTL_SECS);
    sign_jwt(account, &claims)
}

/// Build a signed JWT assertion for the current time.
pub fn build_signed_jwt(account: &ServiceAccount) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before Unix epoch")?
        .as_secs();
    build_signed_jwt_at(account, now)
}

/// Response from `POST {token_uri}` for a JWT bearer grant.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "expires_in", default)]
    pub expires_in: Option<u64>,
    #[serde(rename = "token_type", default)]
    pub token_type: Option<String>,
}

/// `application/x-www-form-urlencoded` fields for the JWT bearer grant.
pub fn token_request_fields(assertion: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
        ),
        ("assertion", assertion.to_string()),
    ]
}

/// Exchange a signed JWT assertion for a Google access token.
///
/// Network call — not exercised by unit tests.
pub async fn fetch_access_token(
    client: &reqwest::Client,
    account: &ServiceAccount,
) -> Result<String> {
    let assertion = build_signed_jwt(account)?;
    let response = client
        .post(account.token_uri())
        .form(&token_request_fields(&assertion))
        .send()
        .await
        .with_context(|| {
            format!("POSTing JWT bearer grant to {}", account.token_uri())
        })?
        .error_for_status()
        .context("token endpoint returned an error status")?;
    let token: TokenResponse = response
        .json()
        .await
        .context("parsing token response JSON")?;
    if token.access_token.is_empty() {
        anyhow::bail!("token endpoint returned an empty access_token");
    }
    Ok(token.access_token)
}

// ── Sheets API values ──────────────────────────────────────────────────────

/// Response from the Sheets API `values.get` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ValuesResponse {
    #[serde(default)]
    pub range: String,
    #[serde(rename = "majorDimension", default)]
    pub major_dimension: String,
    /// Cell values; `values[0]` is the header row when the sheet has data.
    #[serde(default)]
    pub values: Vec<Vec<Value>>,
}

/// Quote a sheet name for a range when it needs escaping (spaces, quotes, `!`).
///
/// Google Sheets range syntax requires single quotes around sheet names that
/// contain spaces or special characters; embedded single quotes are doubled.
pub fn sheet_range(sheet_name: &str) -> String {
    if sheet_name.contains([' ', '\'', '!']) {
        format!("'{}'", sheet_name.replace('\'', "''"))
    } else {
        sheet_name.to_string()
    }
}

/// Build the `values.get` URL for a spreadsheet tab.
///
/// The spreadsheet ID and range are appended as path segments so the URL
/// library percent-encodes them correctly (sheet names may contain spaces).
pub fn values_url(spreadsheet_id: &str, sheet_name: &str) -> Result<Url> {
    let mut url =
        Url::parse(SHEETS_API_BASE).context("parsing Sheets API base URL")?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Sheets API base URL cannot be a base"))?;
    segments.extend([spreadsheet_id, "values", &sheet_range(sheet_name)]);
    drop(segments);
    Ok(url)
}

/// Fetch the values of a spreadsheet tab using an existing access token.
///
/// Network call — not exercised by unit tests.
pub async fn fetch_values(
    client: &reqwest::Client,
    access_token: &str,
    spreadsheet_id: &str,
    sheet_name: &str,
) -> Result<ValuesResponse> {
    let url = values_url(spreadsheet_id, sheet_name)?;
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("GETting spreadsheet values")?
        .error_for_status()
        .context("Sheets API returned an error status")?;
    response
        .json()
        .await
        .context("parsing Sheets API response JSON")
}

/// Fetch a tab's values end-to-end using service-account auth.
///
/// Convenience wrapper: obtains a token then reads the tab. Network calls —
/// not exercised by unit tests.
pub async fn fetch_sheet_values(
    client: &reqwest::Client,
    account: &ServiceAccount,
    spreadsheet_id: &str,
    sheet_name: &str,
) -> Result<ValuesResponse> {
    let token = fetch_access_token(client, account).await?;
    fetch_values(client, &token, spreadsheet_id, sheet_name).await
}

// ── Fetch abstraction (slice 3) ────────────────────────────────────────────

/// Abstraction over the Sheets fetch so tools can be tested without live
/// Google HTTP calls. The production implementation ([`HttpSheetFetcher`])
/// performs service-account auth + `values.get`; tests inject a mock.
#[async_trait]
pub trait SheetFetcher: Send + Sync {
    /// Fetch a spreadsheet tab's values using a loaded service account.
    async fn fetch_sheet(
        &self,
        account: &ServiceAccount,
        spreadsheet_id: &str,
        sheet_name: &str,
    ) -> Result<ValuesResponse>;
}

/// Real fetcher backed by `reqwest` and the JWT bearer grant.
#[derive(Debug, Default)]
pub struct HttpSheetFetcher {
    client: reqwest::Client,
}

impl HttpSheetFetcher {
    /// Create a fetcher with a fresh HTTP client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl SheetFetcher for HttpSheetFetcher {
    async fn fetch_sheet(
        &self,
        account: &ServiceAccount,
        spreadsheet_id: &str,
        sheet_name: &str,
    ) -> Result<ValuesResponse> {
        fetch_sheet_values(&self.client, account, spreadsheet_id, sheet_name)
            .await
    }
}

// ── Cell coercion ──────────────────────────────────────────────────────────

/// Convert a raw Sheets API cell to a trimmed string.
///
/// Google returns text as strings, but numeric cells come back as numbers and
/// formula results such as `=TRUE` come back as booleans; this normalizes all
/// of them to their textual representation.
pub fn cell_to_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Whether a `Done` cell marks the row as finished.
///
/// Accepts a JSON boolean (`true`, from a checkbox or `=TRUE()` formula) or
/// the literal string `TRUE` (case-insensitive), matching the sheet contract
/// `Done != TRUE`.
pub fn cell_is_done(value: Option<&Value>) -> bool {
    match value {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("TRUE"),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(_) => false,
    }
}

// ── Normalization ──────────────────────────────────────────────────────────

/// A single normalized, eligible activity row from the sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedRow {
    /// 1-based position of the row within the data rows (after the header),
    /// i.e. `values[row_number]` in the API response (`1` = first data row,
    /// sheet row 2 when the header is row 1). Preserved through normalization
    /// so duplicate-title and invalid reporting always refers to the original
    /// sheet row even when blank/ineligible/done/invalid rows precede it.
    pub row_number: usize,
    /// External job key: the exact `Title of Activity` cell.
    pub title: String,
    /// Site address/location from the `Site` column.
    pub location: String,
    /// Exact `Type of Activity` value.
    pub activity_type: String,
    /// Start date formatted as `YYYY-MM-DD`.
    pub start_date: Option<String>,
    /// End date formatted as `YYYY-MM-DD`.
    pub end_date: Option<String>,
    /// `Job Leader` cell.
    pub job_leader: String,
    /// `Team Member` cell.
    pub team_member: String,
    /// `Robots` cell.
    pub robots: String,
    /// `Additional Info` cell.
    pub additional_info: String,
    /// Whether the `Done` column marks the row finished.
    pub done: bool,
}

/// A validation conflict detected while normalizing eligible rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowConflict {
    /// An eligible row has a blank `Title of Activity`.
    BlankTitle { row_number: usize },
    /// Two or more eligible rows share the same title.
    DuplicateTitle {
        title: String,
        row_numbers: Vec<usize>,
    },
}

/// An eligible row that could not be normalized (e.g. an unparseable date).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidRow {
    pub row_number: usize,
    pub reason: String,
}

/// Output of [`normalize_rows`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizationResult {
    /// Eligible rows with unique, non-blank titles and valid dates.
    pub rows: Vec<NormalizedRow>,
    /// Validation conflicts (blank or duplicate eligible titles).
    pub conflicts: Vec<RowConflict>,
    /// Eligible rows that failed to normalize (e.g. bad dates).
    pub invalid: Vec<InvalidRow>,
    /// Rows skipped as ineligible, done, or blank.
    pub skipped: usize,
    /// Exact titles of eligible rows currently marked Done (non-blank, deduped).
    ///
    /// Done rows are excluded from `rows` (they are not re-imported), but their
    /// titles are kept so the import service can mark previously imported jobs
    /// completed/ignored.
    pub done_titles: Vec<String>,
}

impl Default for NormalizationResult {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            conflicts: Vec::new(),
            invalid: Vec::new(),
            skipped: 0,
            done_titles: Vec::new(),
        }
    }
}

/// Maps the sheet's header row to column indices for the columns we consume.
#[derive(Debug, Clone)]
struct ColumnMap {
    title: usize,
    location: usize,
    activity_type: usize,
    start_date: usize,
    end_date: usize,
    job_leader: usize,
    team_member: usize,
    robots: usize,
    additional_info: usize,
    done: usize,
}

impl ColumnMap {
    /// Build from a header row using exact, trimmed column-name matches.
    fn from_header(header: &[String]) -> Result<Self> {
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, name) in header.iter().enumerate() {
            let name = name.trim();
            if !name.is_empty() {
                index.entry(name.to_string()).or_insert(i);
            }
        }
        let get = |col: &str| -> Result<usize> {
            index.get(col).copied().with_context(|| {
                format!(
                    "missing required column '{col}' in sheet header (found: {})",
                    header
                        .iter()
                        .map(|h| format!("'{h}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        };
        Ok(Self {
            title: get(COL_TITLE)?,
            location: get(COL_SITE)?,
            activity_type: get(COL_ACTIVITY_TYPE)?,
            start_date: get(COL_START_DATE)?,
            end_date: get(COL_END_DATE)?,
            job_leader: get(COL_JOB_LEADER)?,
            team_member: get(COL_TEAM_MEMBER)?,
            robots: get(COL_ROBOTS)?,
            additional_info: get(COL_ADDITIONAL_INFO)?,
            done: get(COL_DONE)?,
        })
    }

    fn cell<'a>(&self, row: &'a [Value], idx: usize) -> Option<&'a Value> {
        row.get(idx)
    }

    fn title<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.title))
    }
    fn location<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.location))
    }
    fn activity_type<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.activity_type))
    }
    fn start_date<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.start_date))
    }
    fn end_date<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.end_date))
    }
    fn job_leader<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.job_leader))
    }
    fn team_member<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.team_member))
    }
    fn robots<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.robots))
    }
    fn additional_info<'a>(&self, row: &'a [Value]) -> String {
        cell_to_string(self.cell(row, self.additional_info))
    }
    fn done(&self, row: &[Value]) -> bool {
        cell_is_done(self.cell(row, self.done))
    }
}

/// Parse a sheet date in `%-d-%b-%Y` (e.g. `5-Aug-2026`) into `YYYY-MM-DD`.
///
/// An empty cell yields `Ok(None)`; an unparseable cell yields an error.
fn parse_sheet_date(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let date = NaiveDate::parse_from_str(raw, SOURCE_DATE_FORMAT).map_err(|e| {
        format!(
            "cannot parse '{raw}' as {SOURCE_DATE_FORMAT} (expected e.g. 5-Aug-2026): {e}"
        )
    })?;
    Ok(Some(date.format(NORMALIZED_DATE_FORMAT).to_string()))
}

/// Whether a trimmed activity type exactly matches the eligible list.
fn is_eligible(
    activity_type: &str,
    eligible_activity_types: &[String],
) -> bool {
    eligible_activity_types.iter().any(|e| e == activity_type)
}

/// Whether a row is entirely blank.
fn is_blank_row(row: &[Value]) -> bool {
    row.iter().all(|cell| cell_to_string(Some(cell)).is_empty())
}

/// Normalize and filter raw sheet rows.
///
/// Eligibility rules (applied in order):
/// 1. `Type of Activity` must exactly match one of `eligible_activity_types`.
/// 2. `Done` must not be `TRUE`.
/// 3. Fully blank rows are skipped silently.
///
/// Eligible rows are then validated:
/// - A blank `Title of Activity` is reported as a
///   [`RowConflict::BlankTitle`].
/// - Two or more eligible rows sharing the same title are reported as a
///   [`RowConflict::DuplicateTitle`]; the affected rows are excluded from the
///   output because the title cannot be used as a stable import key.
/// - Rows with unparseable dates are reported in `invalid` and excluded.
///
/// `row_number` in the result is 1-based within `rows` (1 = first data row,
/// i.e. sheet row 2 when the header is row 1). [`NormalizedRow::row_number`]
/// carries that original position, so [`RowConflict::DuplicateTitle`] and
/// [`InvalidRow`] always report the source row even when blank/ineligible/
/// done/invalid rows precede them.
pub fn normalize_rows(
    header: &[String],
    rows: &[Vec<Value>],
    eligible_activity_types: &[String],
) -> Result<NormalizationResult> {
    let map = ColumnMap::from_header(header)?;

    let mut normalized: Vec<NormalizedRow> = Vec::new();
    let mut conflicts: Vec<RowConflict> = Vec::new();
    let mut invalid: Vec<InvalidRow> = Vec::new();
    let mut done_titles: Vec<String> = Vec::new();
    let mut skipped: usize = 0;

    for (idx, row) in rows.iter().enumerate() {
        let row_number = idx + 1;

        if is_blank_row(row) {
            skipped += 1;
            continue;
        }

        let activity_type = map.activity_type(row);
        if !is_eligible(&activity_type, eligible_activity_types) {
            skipped += 1;
            continue;
        }

        if map.done(row) {
            // Done rows are not imported, but their titles feed the done
            // transition so previously imported jobs can be completed locally.
            let title = map.title(row);
            if !title.is_empty() {
                done_titles.push(title);
            }
            skipped += 1;
            continue;
        }

        let title = map.title(row);
        if title.is_empty() {
            conflicts.push(RowConflict::BlankTitle { row_number });
            continue;
        }

        let start_date = match parse_sheet_date(&map.start_date(row)) {
            Ok(d) => d,
            Err(reason) => {
                invalid.push(InvalidRow {
                    row_number,
                    reason: format!("{COL_START_DATE}: {reason}"),
                });
                continue;
            }
        };
        let end_date = match parse_sheet_date(&map.end_date(row)) {
            Ok(d) => d,
            Err(reason) => {
                invalid.push(InvalidRow {
                    row_number,
                    reason: format!("{COL_END_DATE}: {reason}"),
                });
                continue;
            }
        };

        normalized.push(NormalizedRow {
            row_number,
            title,
            location: map.location(row),
            activity_type,
            start_date,
            end_date,
            job_leader: map.job_leader(row),
            team_member: map.team_member(row),
            robots: map.robots(row),
            additional_info: map.additional_info(row),
            done: false,
        });
    }

    // Detect duplicate eligible titles; excluded rows must not be imported.
    // `row_numbers` uses each row's original source position (preserved on
    // `NormalizedRow`), so earlier blank/ineligible/done/invalid rows never
    // skew the reported numbers.
    let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in normalized.iter().enumerate() {
        by_title.entry(row.title.clone()).or_default().push(i);
    }
    let mut excluded: Vec<bool> = vec![false; normalized.len()];
    let mut duplicate_titles: Vec<String> = by_title
        .iter()
        .filter(|(_, idxs)| idxs.len() > 1)
        .map(|(title, _)| title.clone())
        .collect();
    duplicate_titles.sort();
    for title in duplicate_titles {
        let idxs = &by_title[&title];
        conflicts.push(RowConflict::DuplicateTitle {
            title: title.clone(),
            row_numbers: idxs
                .iter()
                .map(|&i| normalized[i].row_number)
                .collect(),
        });
        for &i in idxs {
            excluded[i] = true;
        }
    }

    let rows = normalized
        .into_iter()
        .zip(excluded)
        .filter_map(|(row, excluded)| (!excluded).then_some(row))
        .collect();

    done_titles.sort();
    done_titles.dedup();

    Ok(NormalizationResult {
        rows,
        conflicts,
        invalid,
        skipped,
        done_titles,
    })
}

/// Normalize a full [`ValuesResponse`]: treats `values[0]` as the header.
pub fn normalize_response(
    response: &ValuesResponse,
    eligible_activity_types: &[String],
) -> Result<NormalizationResult> {
    let Some(header_row) = response.values.first() else {
        return Ok(NormalizationResult::default());
    };
    let header: Vec<String> =
        header_row.iter().map(|c| cell_to_string(Some(c))).collect();
    normalize_rows(&header, &response.values[1..], eligible_activity_types)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A synthetic RSA private key used ONLY by unit tests. This is not a
    /// real credential; tests never read the user's service-account file.
    const TEST_RSA_PRIVATE_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDTlMBStYYnFG1Y
f7a6SRZZtIczQkV8hm4/8R+cCs0xDS6dfPNTcv63ObbEA/mhK1m0ttx1NCEJqgCO
lFAucmFxQ6txdR2igs8+K31QfCvEN1KVCnh9OuKcI74NeEGAaXF5Ifsw38TrO1vN
LKUcX+Zhmt74dZy3TtxWsgJJup3h00YG6zKnJOyPaxuyqDGrQM3pljvNhFUwE67w
NuqbKipckzh87q8WpiybEl1inYV3f6TSA8bT5duc2cJtcPJNDO4+UvqHjltnECqN
Ol/V81rKeCrQVyhWbVZmRwfUbfzrj3tjp+xkLAhnllCNLh2K6GewJipKP3HIZrFx
b9Q+Z5dVAgMBAAECggEAEcE0SFyCNupQaLJC56v1a66p/8OqaBIx0zzNLb98S4bz
J/URyClotYBq1VUOEBe+rdsAcfdfZFu3Mz+/4L3pbmNC0kEFLRtz/6APon7g/1Sz
Id2gkcWsuXSIYMH8ISX4BzWySf4lHKY9BGEgM9raTst7tXbNIVbFR6MlbQFzeT0l
3aVuMy/VYmHWtqnn8yGYC9ezlC3oj/pzyEtnaaAhbjWCpe0fkjWBDXI1px7yCqCy
MOApQlKP8oasw0MEIp69qUZpY02vUyu1uIUGwXEi92x//s/DzGgyD+a6mEOtgl7s
fsqyBnAKdI8yTmL/ahTZenBzlhCXcmPKixYtmrdgMQKBgQDrhb5fePwKtAha8MwJ
mkb/+MJc0UBeqzoQ13hjH4Z2YmHGdR+rC1OfIo2s83QP2LDhwlm5hPOfSIVkLo3u
NJLOx4opqE3lVNY5LtP3VfWCvUP6E0z5pWP4sfmqOzGDMnGKRNU859uKNS8O7vv/
1zGF6v2LolS51DDEeQJqsi4HGQKBgQDl+h7j9L01oukdOmUTvuImIc9EKq0VjLFo
5vNgcx5tvcY/huxXm9UcwMZF6cTth2fHeuX6etPfaYEE5X+XNaVsno7IvTh40wiU
+u9kIDKE6Tj3Kk2UylA3UdATy1uI324zJB7rS7amtra5D4G/nxShVPopd2Frc02x
QLtlvE/FnQKBgQCGIP3BC3qmcc8MU3Qvx8/FeRrflz/MakFAVCW4dbyy8OZ0CkHF
vEacKyZ6J4+icqqRd4h3sfK4dKma2zRzQzeUUWkqvjHWeBEkMbn/ctHF6hmrcpB0
4C7l9B2WR+2zpOeqcfbqn7SUqiMpowqasif+90v72K/dwK0hRzUMJHs4CQKBgGnO
kbe/Oe4bbbUM0MQs5k807u8l00w+1sC0wPR3AmDrFvLTWJlWEM6RwqcXzoqZ6Z1V
ZcnACQqYt8tQ60reW6WFrZudswWj0ib47HrcdWHBC3xr8hWqnw1Ujq8MuKhYY5MT
40XOJ9K77YVnJQLMZelz90RssF2HRw9uAMnlwa3hAoGAeXekbVS9cXnzJaT31YJc
wIIauuogNbzDqPwI+M50qbGyEZb/7HqtOd2isnl5Gx7X590NL00TP1buiT4Kqi/Z
LOf5UUl3gy5s3RKcblYhjxPSu+Pm1WXW+sGXcuAM2kcKOvJM6K5toYrDr6iHGKiT
jctMViKxBZWEptJ9D6JwRDg=
-----END PRIVATE KEY-----"#;

    /// Public half of `TEST_RSA_PRIVATE_KEY_PEM`, used to verify JWTs in
    /// tests (decoding requires a public key in PKCS#8 form).
    const TEST_RSA_PUBLIC_KEY_PEM: &str = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA05TAUrWGJxRtWH+2ukkW
WbSHM0JFfIZuP/EfnArNMQ0unXzzU3L+tzm2xAP5oStZtLbcdTQhCaoAjpRQLnJh
cUOrcXUdooLPPit9UHwrxDdSlQp4fTrinCO+DXhBgGlxeSH7MN/E6ztbzSylHF/m
YZre+HWct07cVrICSbqd4dNGBusypyTsj2sbsqgxq0DN6ZY7zYRVMBOu8Dbqmyoq
XJM4fO6vFqYsmxJdYp2Fd3+k0gPG0+XbnNnCbXDyTQzuPlL6h45bZxAqjTpf1fNa
yngq0FcoVm1WZkcH1G386497Y6fsZCwIZ5ZQjS4diuhnsCYqSj9xyGaxcW/UPmeX
VQIDAQAB
-----END PUBLIC KEY-----"#;

    fn test_account() -> ServiceAccount {
        ServiceAccount {
            account_type: "service_account".into(),
            project_id: "test-project".into(),
            private_key_id: "key-123".into(),
            private_key: TEST_RSA_PRIVATE_KEY_PEM.into(),
            client_email: "test@test-project.iam.gserviceaccount.com".into(),
            client_id: "client-456".into(),
            token_uri: None,
        }
    }

    fn rows(v: serde_json::Value) -> Vec<Vec<Value>> {
        serde_json::from_value(v).unwrap()
    }

    fn default_header() -> Vec<String> {
        [
            COL_OFFICE,
            COL_ACTIVITY_TYPE,
            COL_TITLE,
            COL_SITE,
            COL_START_DATE,
            COL_END_DATE,
            COL_JOB_LEADER,
            COL_TEAM_MEMBER,
            COL_ROBOTS,
            COL_ADDITIONAL_INFO,
            COL_DONE,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn default_eligible() -> Vec<String> {
        vec!["Boris Job".into(), "Demo".into()]
    }

    fn data_rows() -> serde_json::Value {
        json!([
            [
                "Shenzhen",
                "Boris Job",
                "Site A install",
                "100 Bay St",
                "5-Aug-2026",
                "12-Aug-2026",
                "Alice",
                "Bob",
                "R1",
                "bring drill",
                ""
            ],
            [
                "Shenzhen",
                "Demo",
                "Customer demo",
                "200 Main Rd",
                "1-Sep-2026",
                "",
                "Alice",
                "",
                "R2, R3",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Travel",
                "Flight to site",
                "300 Airport Rd",
                "",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
        ])
    }

    // ── Service account ────────────────────────────────────────────────────

    #[test]
    fn service_account_parses_google_json() {
        let json = json!({
            "type": "service_account",
            "project_id": "p",
            "private_key_id": "kid",
            "private_key": "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "client_id": "123",
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string();
        let account = ServiceAccount::from_json_str(&json).unwrap();
        assert_eq!(account.account_type, "service_account");
        assert_eq!(account.client_email, "sa@p.iam.gserviceaccount.com");
        assert_eq!(
            account.private_key,
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n"
        );
        assert_eq!(account.token_uri(), "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn service_account_defaults_token_uri_to_google() {
        let account = test_account();
        assert_eq!(account.token_uri(), GOOGLE_TOKEN_URI);
    }

    #[test]
    fn service_account_missing_required_fields_fails() {
        let json = json!({ "type": "service_account" }).to_string();
        assert!(ServiceAccount::from_json_str(&json).is_err());
    }

    #[test]
    fn service_account_file_load_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sa.json");
        std::fs::write(
            &path,
            json!({
                "type": "service_account",
                "private_key": "x",
                "client_email": "sa@example.com",
            })
            .to_string(),
        )
        .unwrap();
        let account = ServiceAccount::from_json_file(&path).unwrap();
        assert_eq!(account.client_email, "sa@example.com");

        let missing = dir.path().join("nope.json");
        assert!(ServiceAccount::from_json_file(&missing).is_err());
    }

    // ── JWT signing ────────────────────────────────────────────────────────

    #[test]
    fn jwt_roundtrip_has_expected_claims() {
        let account = test_account();
        let assertion = build_signed_jwt_at(&account, 1_700_000_000).unwrap();

        let decoding = jsonwebtoken::DecodingKey::from_rsa_pem(
            TEST_RSA_PUBLIC_KEY_PEM.as_bytes(),
        )
        .unwrap();
        let mut validation =
            jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        // Claims are checked manually below; disable default leeway checks.
        validation.validate_exp = false;
        validation.validate_aud = false;
        let data = jsonwebtoken::decode::<serde_json::Value>(
            &assertion,
            &decoding,
            &validation,
        )
        .unwrap();
        let claims = data.claims;
        assert_eq!(claims["iss"], "test@test-project.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], GOOGLE_TOKEN_URI);
        assert_eq!(claims["scope"], SHEETS_READONLY_SCOPE);
        assert_eq!(claims["iat"], 1_700_000_000);
        assert_eq!(claims["exp"], 1_700_000_000 + JWT_TTL_SECS);
    }

    #[test]
    fn jwt_rejects_invalid_private_key() {
        let mut account = test_account();
        account.private_key = "not a pem key".into();
        let result = build_signed_jwt_at(&account, 1_700_000_000);
        assert!(result.is_err());
    }

    #[test]
    fn token_request_fields_are_form_encoded_pairs() {
        let fields = token_request_fields("assertion-token");
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields[0],
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string()
            )
        );
        assert_eq!(fields[1], ("assertion", "assertion-token".to_string()));
    }

    // ── URL building ───────────────────────────────────────────────────────

    #[test]
    fn values_url_plain_sheet_name() {
        let url = values_url("abc123", "US").unwrap();
        assert_eq!(
            url.as_str(),
            "https://sheets.googleapis.com/v4/spreadsheets/abc123/values/US"
        );
    }

    #[test]
    fn values_url_quotes_and_encodes_sheet_name_with_spaces() {
        let url = values_url("abc123", "Master Jobs").unwrap();
        let path = url.path();
        assert!(path.starts_with("/v4/spreadsheets/abc123/values/"));
        assert!(path.contains("Master"));
        assert!(path.contains("Jobs"));
        // Spaces must be percent-encoded; the tab name is single-quoted.
        assert!(path.contains("%20"));
        assert!(path.contains('\''));
    }

    #[test]
    fn values_url_escapes_embedded_quotes() {
        let range = sheet_range("It's Here");
        assert_eq!(range, "'It''s Here'");
    }

    // ── Cell coercion ──────────────────────────────────────────────────────

    #[test]
    fn cell_to_string_handles_all_types() {
        assert_eq!(cell_to_string(None), "");
        assert_eq!(cell_to_string(Some(&Value::Null)), "");
        assert_eq!(cell_to_string(Some(&json!(" hi "))), "hi");
        assert_eq!(cell_to_string(Some(&json!(42))), "42");
        assert_eq!(cell_to_string(Some(&json!(true))), "true");
    }

    #[test]
    fn cell_is_done_variants() {
        assert!(!cell_is_done(None));
        assert!(!cell_is_done(Some(&json!(""))));
        assert!(cell_is_done(Some(&json!("TRUE"))));
        assert!(cell_is_done(Some(&json!("true"))));
        assert!(cell_is_done(Some(&json!(true))));
        assert!(!cell_is_done(Some(&json!("false"))));
        assert!(!cell_is_done(Some(&json!(false))));
        assert!(!cell_is_done(Some(&json!("Done"))));
    }

    // ── Date parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_sheet_date_formats_to_iso() {
        assert_eq!(
            parse_sheet_date("5-Aug-2026").unwrap(),
            Some("2026-08-05".into())
        );
        // Padded day is accepted by chrono's `%-d` too.
        assert_eq!(
            parse_sheet_date("05-Aug-2026").unwrap(),
            Some("2026-08-05".into())
        );
        assert_eq!(parse_sheet_date("  ").unwrap(), None);
    }

    #[test]
    fn parse_sheet_date_rejects_other_formats() {
        assert!(parse_sheet_date("5/8/2026").is_err());
        assert!(parse_sheet_date("not-a-date").is_err());
    }

    // ── Normalization ──────────────────────────────────────────────────────

    #[test]
    fn normalize_rows_keeps_eligible_and_skips_ineligible() {
        let result = normalize_rows(
            &default_header(),
            &rows(data_rows()),
            &default_eligible(),
        )
        .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.skipped, 2);
        assert!(result.conflicts.is_empty());
        assert!(result.invalid.is_empty());

        let row = &result.rows[0];
        assert_eq!(row.title, "Site A install");
        assert_eq!(row.location, "100 Bay St");
        assert_eq!(row.activity_type, "Boris Job");
        assert_eq!(row.start_date.as_deref(), Some("2026-08-05"));
        assert_eq!(row.end_date.as_deref(), Some("2026-08-12"));
        assert_eq!(row.job_leader, "Alice");
        assert_eq!(row.team_member, "Bob");
        assert_eq!(row.robots, "R1");
        assert_eq!(row.additional_info, "bring drill");
        assert!(!row.done);
    }

    #[test]
    fn normalize_rows_done_rows_are_skipped() {
        let r = rows(json!([
            [
                "Shenzhen",
                "Boris Job",
                "Finished job",
                "1 A St",
                "5-Aug-2026",
                "6-Aug-2026",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Active job",
                "2 B St",
                "7-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
        ]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].title, "Active job");
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn normalize_rows_reports_done_titles() {
        let r = rows(json!([
            [
                "Shenzhen",
                "Boris Job",
                "Finished job",
                "1 A St",
                "5-Aug-2026",
                "6-Aug-2026",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Also done",
                "2 B St",
                "7-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Also done",
                "3 C St",
                "8-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Boris Job",
                "",
                "4 D St",
                "9-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
        ]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        // Done titles are collected, deduped, and sorted; blank titles omitted.
        assert_eq!(result.done_titles, vec!["Also done", "Finished job"]);
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped, 4);
    }

    #[test]
    fn normalize_rows_reports_blank_title_conflict() {
        let r = rows(json!([[
            "Shenzhen",
            "Boris Job",
            "",
            "3 C St",
            "5-Aug-2026",
            "",
            "",
            "",
            "",
            "",
            ""
        ],]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert!(result.rows.is_empty());
        assert_eq!(
            result.conflicts,
            vec![RowConflict::BlankTitle { row_number: 1 }]
        );
    }

    #[test]
    fn normalize_rows_reports_duplicate_titles_and_excludes_both() {
        let r = rows(json!([
            [
                "Shenzhen",
                "Boris Job",
                "Shared title",
                "1 A St",
                "5-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Demo",
                "Shared title",
                "2 B St",
                "6-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Unique title",
                "3 C St",
                "7-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
        ]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].title, "Unique title");
        assert_eq!(
            result.conflicts,
            vec![RowConflict::DuplicateTitle {
                title: "Shared title".into(),
                row_numbers: vec![1, 2],
            }]
        );
    }

    #[test]
    fn normalize_rows_duplicate_row_numbers_survive_preceding_skips() {
        // Blank, ineligible, done, and invalid rows all precede the duplicate
        // titles. The duplicate conflict must report the ORIGINAL source row
        // numbers (within the data rows after the header), not positions in
        // the filtered normalized list.
        let r = rows(json!([
            ["", "", "", "", "", "", "", "", "", "", ""],
            [
                "Shenzhen",
                "Travel",
                "Flight to site",
                "300 Airport Rd",
                "",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Finished job",
                "1 A St",
                "5-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                "TRUE"
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Bad date",
                "1 A St",
                "not-a-date",
                "5-Aug-2026",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Shared title",
                "1 A St",
                "5-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Demo",
                "Shared title",
                "2 B St",
                "6-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
            [
                "Shenzhen",
                "Boris Job",
                "Unique title",
                "3 C St",
                "7-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
        ]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert_eq!(result.skipped, 3);
        assert_eq!(result.invalid.len(), 1);
        assert_eq!(result.invalid[0].row_number, 4);
        assert!(
            result.invalid[0].reason.contains("Start Date")
                && result.invalid[0].reason.contains("not-a-date"),
            "reason: {}",
            result.invalid[0].reason
        );
        // Only the unique row survives; it keeps its original row number 7.
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].title, "Unique title");
        assert_eq!(result.rows[0].row_number, 7);
        // Duplicate rows were at source rows 5 and 6, not 1 and 2.
        assert_eq!(
            result.conflicts,
            vec![RowConflict::DuplicateTitle {
                title: "Shared title".into(),
                row_numbers: vec![5, 6],
            }]
        );
    }

    #[test]
    fn normalize_rows_skips_blank_rows() {
        let r = rows(json!([
            ["", "", "", "", "", "", "", "", "", "", ""],
            [
                "Shenzhen",
                "Boris Job",
                "Real job",
                "1 A St",
                "5-Aug-2026",
                "",
                "",
                "",
                "",
                "",
                ""
            ],
        ]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn normalize_rows_reports_invalid_dates() {
        let r = rows(json!([[
            "Shenzhen",
            "Boris Job",
            "Bad date",
            "1 A St",
            "not-a-date",
            "5-Aug-2026",
            "",
            "",
            "",
            "",
            ""
        ],]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert!(result.rows.is_empty());
        assert_eq!(result.invalid.len(), 1);
        assert_eq!(result.invalid[0].row_number, 1);
        assert!(
            result.invalid[0].reason.contains("Start Date")
                || result.invalid[0].reason.contains("start")
        );
    }

    #[test]
    fn normalize_rows_accepts_empty_dates() {
        let r = rows(json!([[
            "Shenzhen", "Demo", "No dates", "1 A St", "", "", "", "", "", "",
            ""
        ],]));
        let result =
            normalize_rows(&default_header(), &r, &default_eligible()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].start_date, None);
        assert_eq!(result.rows[0].end_date, None);
    }

    #[test]
    fn normalize_rows_respects_custom_eligible_types() {
        let r = rows(json!([[
            "Shenzhen",
            "Installation",
            "Install job",
            "1 A St",
            "5-Aug-2026",
            "",
            "",
            "",
            "",
            "",
            ""
        ],]));
        let eligible = vec!["Installation".to_string()];
        let result = normalize_rows(&default_header(), &r, &eligible).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].activity_type, "Installation");
    }

    #[test]
    fn normalize_rows_missing_required_column_fails() {
        let header = vec!["Title of Activity".to_string(), "Site".to_string()];
        let r = rows(json!([["X", "Y"]]));
        let result = normalize_rows(&header, &r, &default_eligible());
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Type of Activity"), "err: {err}");
    }

    #[test]
    fn normalize_response_uses_first_row_as_header() {
        let mut all = data_rows();
        let values = all.as_array_mut().unwrap();
        values.insert(
            0,
            Value::Array(
                default_header().into_iter().map(Value::from).collect(),
            ),
        );
        let response = ValuesResponse {
            range: "US".into(),
            major_dimension: "ROWS".into(),
            values: rows(Value::Array(values.clone())),
        };
        let result =
            normalize_response(&response, &default_eligible()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].title, "Site A install");
    }

    #[test]
    fn normalize_response_empty_sheet_is_empty_result() {
        let response = ValuesResponse {
            range: "US".into(),
            major_dimension: "ROWS".into(),
            values: vec![],
        };
        let result =
            normalize_response(&response, &default_eligible()).unwrap();
        assert!(result.rows.is_empty());
        assert!(result.conflicts.is_empty());
        assert_eq!(result.skipped, 0);
    }
}
