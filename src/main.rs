//! # ia-get
//!
//! A command-line tool for downloading files from the Internet Archive.
//!
//! This tool takes an archive.org details URL and downloads all associated files,
//! with support for resumable downloads and MD5 hash verification.

use clap::Parser;
use colored::*;
use ia_get::archive_metadata::{parse_xml_files, XmlFiles};
use ia_get::constants::USER_AGENT;
use ia_get::downloader;
use ia_get::utils::{create_spinner, sanitize_filename, validate_archive_url};
use ia_get::{IaGetError, Result};
use indicatif::ProgressStyle;
use reqwest::{cookie::Jar, Client, StatusCode};
use serde::Deserialize;
use std::sync::Arc;

/// Extended timeout for large file downloads (10 minutes for connection, no read timeout)
const CONNECTION_TIMEOUT_SECS: u64 = 600;

/// Archive.org XAuthn login endpoint (form-encoded email + password).
/// Same flow as the official internetarchive client; answers with the
/// session cookies in the JSON body.
const XAUTHN_LOGIN_API_URL: &str = "https://archive.org/services/xauthn/?op=login";

/// Session cookie identifying the account, authorises restricted downloads
const LOGGED_IN_USER_COOKIE: &str = "logged-in-user";
/// Session cookie signing the account name
const LOGGED_IN_SIG_COOKIE: &str = "logged-in-sig";

/// Archive.org takes a moment to honour fresh session cookies across its
/// download cluster; the internetarchive client waits for the same reason.
const AUTH_PROPAGATION_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Deserialize)]
struct XAuthnResponse {
    success: bool,
    #[serde(default)]
    values: Option<XAuthnValues>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct XAuthnValues {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    cookies: Option<XAuthnCookies>,
}

#[derive(Deserialize)]
struct XAuthnCookies {
    #[serde(rename = "logged-in-user", default)]
    logged_in_user: Option<String>,
    #[serde(rename = "logged-in-sig", default)]
    logged_in_sig: Option<String>,
}

/// Session cookies authorising downloads of restricted archive.org items
#[derive(Debug)]
struct LoginSession {
    logged_in_user: String,
    logged_in_sig: String,
}

impl LoginSession {
    /// Cookie strings valid for archive.org and its download subdomains
    fn cookie_strings(&self) -> Vec<String> {
        vec![
            session_cookie(LOGGED_IN_USER_COOKIE, &self.logged_in_user),
            session_cookie(LOGGED_IN_SIG_COOKIE, &self.logged_in_sig),
        ]
    }
}

/// Formats a session cookie so it is sent to archive.org and its
/// download subdomains (e.g. ia123456.us.archive.org)
fn session_cookie(name: &str, value: &str) -> String {
    format!("{name}={value}; Domain=.archive.org; Path=/; Secure")
}

/// Extracts the download session cookies from a successful XAuthn response
fn parse_login_session(response: &XAuthnResponse) -> Result<LoginSession> {
    response
        .values
        .as_ref()
        .and_then(|values| values.cookies.as_ref())
        .and_then(
            |cookies| match (&cookies.logged_in_user, &cookies.logged_in_sig) {
                (Some(user), Some(sig)) => Some(LoginSession {
                    logged_in_user: user.clone(),
                    logged_in_sig: sig.clone(),
                }),
                _ => None,
            },
        )
        .ok_or_else(|| IaGetError::Network("Login response missing session cookies".to_string()))
}

/// Builds the HTTP client used for metadata and file downloads
///
/// The cookie jar is owned by the caller so authenticated session cookies
/// can be added to it after login.
fn build_http_client(cookie_jar: Arc<Jar>) -> Result<Client> {
    let builder = Client::builder()
        .user_agent(USER_AGENT)
        .cookie_provider(cookie_jar)
        .connect_timeout(std::time::Duration::from_secs(CONNECTION_TIMEOUT_SECS))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(1)
        .tcp_keepalive(std::time::Duration::from_secs(60));

    Ok(builder.build()?)
}

/// Checks if a URL is accessible by sending a HEAD request
async fn is_url_accessible(url: &str, client: &Client) -> Result<()> {
    let response = client
        .head(url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

/// Converts a details URL to the corresponding XML files list URL
///
/// Takes an archive.org details URL and converts it to the XML metadata URL
/// by replacing "details" with "download" and appending "_files.xml"
///
/// # Arguments
/// * `original_url` - The archive.org details URL
///
/// # Returns
/// The corresponding XML files list URL
fn get_xml_url(original_url: &str) -> String {
    // Remove trailing slash if present to get a consistent base for identifier extraction
    let trimmed_url = original_url.trim_end_matches('/');

    // The identifier is the last segment of the trimmed URL
    // This expect is considered safe because get_xml_url is only called after
    // validate_archive_url has confirmed the URL structure.
    let identifier = trimmed_url
        .rsplit('/')
        .next() // Changed from split().last() to address clippy warning
        .expect("Validated URL should have a valid identifier segment after validation");

    // The base URL for download is "https://archive.org/download/{identifier}"
    let download_url_base = format!("https://archive.org/download/{}", identifier);

    // The XML URL is "{download_url_base}/{identifier}_files.xml"
    format!("{}/{}_files.xml", download_url_base, identifier)
}

/// Percent-encodes archive metadata paths for use as URL paths.
///
/// Archive.org metadata gives us literal file paths, not URL-encoded paths. Keep
/// `/` as a directory separator, but encode each path segment so characters like
/// `%`, spaces, `?`, `#`, brackets, and parentheses cannot be interpreted as URL
/// syntax.
fn encode_archive_path_for_url(path: &str) -> String {
    path.split('/')
        .map(encode_archive_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_archive_path_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(segment.len());

    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }

    encoded
}

/// Fetches and parses XML metadata from archive.org
///
/// Combines XML URL generation, accessibility check, download, and parsing
/// into a single operation with integrated error handling.
///
/// # Arguments
/// * `details_url` - The original archive.org details URL
/// * `client` - HTTP client for requests
/// * `spinner` - Progress spinner to update during processing
///
/// # Returns
/// Tuple of (XmlFiles, base_url) for download processing
async fn fetch_xml_metadata(
    details_url: &str,
    client: &Client,
    spinner: &indicatif::ProgressBar,
) -> Result<(XmlFiles, reqwest::Url)> {
    // Generate XML URL
    let xml_url = get_xml_url(details_url);
    spinner.set_message(format!(
        "{} Accessing XML metadata: {}",
        "⚙".blue(),
        xml_url.bold()
    ));

    // Check XML URL accessibility
    if let Err(e) = is_url_accessible(&xml_url, client).await {
        spinner.finish_with_message(format!(
            "{} XML metadata not accessible: {}",
            "✘".red().bold(),
            xml_url.bold()
        ));
        return Err(e); // Propagate the error
    }

    spinner.set_message(format!(
        "{} {}",
        "⚙".blue(),
        "Parsing archive metadata...".bold()
    ));

    // Parse base URL and fetch XML content
    let base_url = reqwest::Url::parse(&xml_url)?;
    let response = client.get(&xml_url).send().await?;
    let xml_content = response.text().await?;

    // Parse XML content with improved error handling
    let files = parse_xml_files(&xml_content)?;

    Ok((files, base_url))
}

/// Command-line interface for ia-get
#[derive(Parser)]
#[command(name = "ia-get")]
#[command(about = "A command-line tool for downloading files from the Internet Archive")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(author = env!("CARGO_PKG_AUTHORS"))]
#[command(
    after_help = "Examples:\n  ia-get https://archive.org/details/deftributetozzap64\n  printf '%s' \"$IA_GET_PASSWORD\" | ia-get --username me@example.com --password-stdin https://archive.org/details/En-ROMs"
)]
struct Cli {
    /// URL to an archive.org details page
    url: String,

    /// Archive.org account email address for authenticated downloads
    #[arg(long)]
    username: Option<String>,

    /// Archive.org password (use with --username)
    #[arg(long, requires = "username", conflicts_with = "password_stdin")]
    password: Option<String>,

    /// Read archive.org password from stdin (use with --username)
    #[arg(long, requires = "username", conflicts_with = "password")]
    password_stdin: bool,
}

/// Removes only trailing CR/LF from secrets read from stdin
fn trim_trailing_newlines(mut value: String) -> String {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    value
}

/// Resolves optional authentication credentials from CLI flags
fn resolve_auth_credentials(cli: &Cli) -> Result<Option<(String, String)>> {
    let Some(username) = cli.username.as_ref() else {
        return Ok(None);
    };

    if username.trim().is_empty() {
        return Err(IaGetError::Network(
            "Authentication username cannot be empty.".to_string(),
        ));
    }

    let password = if let Some(password) = cli.password.as_ref() {
        password.clone()
    } else if cli.password_stdin {
        let mut stdin_input = String::new();
        std::io::stdin().read_line(&mut stdin_input)?;
        trim_trailing_newlines(stdin_input)
    } else {
        return Err(IaGetError::Network(
            "Authentication requires --password or --password-stdin when --username is set."
                .to_string(),
        ));
    };

    if password.is_empty() {
        return Err(IaGetError::Network(
            "Authentication password cannot be empty.".to_string(),
        ));
    }

    Ok(Some((username.clone(), password)))
}

/// Builds a descriptive authentication error from archive.org XAuthn responses
fn describe_xauthn_failure(status: StatusCode, response: &XAuthnResponse) -> String {
    let reason = response
        .values
        .as_ref()
        .and_then(|values| values.reason.as_deref())
        .map(|reason| match reason {
            "account_not_found" => "Account not found, check your email and try again.".to_string(),
            "account_bad_password" => "Incorrect password, try again.".to_string(),
            other => other.to_string(),
        })
        .or_else(|| response.error.clone())
        .unwrap_or_else(|| "Unknown authentication error".to_string());

    format!("Authentication failed (HTTP {status}): {reason}")
}

/// Authenticates with archive.org and stores session cookies in the shared cookie jar
///
/// Archive.org retired the login API that served CSRF tokens (it now answers
/// 405), so the XAuthn endpoint is used instead. Unlike a browser login it
/// returns the session cookies in the JSON body, hence the manual insertion
/// into the cookie jar.
async fn authenticate_archive_org(
    client: &Client,
    cookie_jar: &Jar,
    username: &str,
    password: &str,
) -> Result<()> {
    let login_response = client
        .post(XAUTHN_LOGIN_API_URL)
        .timeout(std::time::Duration::from_secs(60))
        .form(&[("email", username), ("password", password)])
        .send()
        .await?;

    let status = login_response.status();
    let payload: XAuthnResponse = login_response.json().await.map_err(|e| {
        IaGetError::Network(format!("Unexpected login response (HTTP {status}): {e}"))
    })?;

    if !payload.success {
        return Err(IaGetError::Network(describe_xauthn_failure(
            status, &payload,
        )));
    }

    let session = parse_login_session(&payload)?;
    let archive_org_url = reqwest::Url::parse("https://archive.org/")?;

    for cookie in session.cookie_strings() {
        cookie_jar.add_cookie_str(&cookie, &archive_org_url);
    }

    // Give archive.org time to propagate the fresh session across its
    // download cluster before the first restricted request.
    tokio::time::sleep(AUTH_PROPAGATION_DELAY).await;

    Ok(())
}

/// Main application entry point
///
/// Parses command line arguments, optionally authenticates to archive.org, validates
/// the archive URL, downloads XML metadata, and initiates file downloads with
/// built-in signal handling.
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let auth_credentials = resolve_auth_credentials(&cli)?;
    let is_authenticated = auth_credentials.is_some();
    let cookie_jar = Arc::new(Jar::default());
    let client = build_http_client(cookie_jar.clone())?;

    // Start a single spinner for the entire initialization process
    let spinner = create_spinner(&format!("Processing archive.org URL: {}", cli.url.bold()));

    // Validate URL format using consolidated function
    if let Err(e) = validate_archive_url(&cli.url) {
        spinner.finish_with_message(format!("{} {}", "✘".red().bold(), e));
        return Err(e.into());
    }

    // Authenticate if credentials were provided
    if let Some((username, password)) = auth_credentials.as_ref() {
        spinner.set_message(format!(
            "{} Authenticating with archive.org as {}...",
            "⚙".blue(),
            username.bold()
        ));

        if let Err(e) = authenticate_archive_org(&client, &cookie_jar, username, password).await {
            spinner.finish_with_message(format!(
                "{} Authentication failed for {}",
                "✘".red().bold(),
                username.bold()
            ));
            return Err(e.into());
        }
    }

    // Check URL accessibility
    if let Err(e) = is_url_accessible(&cli.url, &client).await {
        spinner.finish_with_message(format!(
            "{} Archive.org URL not accessible: {}",
            "✘".red().bold(),
            cli.url.bold()
        ));
        return Err(e.into()); // Propagate error
    }

    // Fetch and parse XML metadata in one operation
    let (files, base_url) = fetch_xml_metadata(&cli.url, &client, &spinner).await?;

    // Prepare download data for batch processing
    let mut sanitized_count = 0;
    let mut private_count = 0;
    let mut sanitized_files: Vec<(String, String)> = Vec::new();
    let mut download_data: Vec<(String, String, Option<String>)> = Vec::new();

    for file in files.files {
        let is_private = file
            .private
            .as_deref()
            .map(|value| value.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if is_private && !is_authenticated {
            private_count += 1;
            continue;
        }

        let encoded_remote_path = encode_archive_path_for_url(&file.name);
        let absolute_url = base_url.join(&encoded_remote_path)?;

        // Sanitize filename for filesystem compatibility
        let (sanitized_name, was_modified) = sanitize_filename(&file.name);

        if was_modified {
            sanitized_files.push((file.name.clone(), sanitized_name.clone()));
            sanitized_count += 1;
        }

        download_data.push((absolute_url.to_string(), sanitized_name, file.md5));
    }

    if download_data.is_empty() {
        if private_count > 0 {
            spinner.finish_with_message(format!(
                "{} No downloadable files found ({} private/restricted)",
                "✘".red().bold(),
                private_count.to_string().bold()
            ));
            return Err(IaGetError::Network(
                "No downloadable files found. This archive may only contain private or restricted files. Try again with --username and --password (or --password-stdin)."
                    .to_string(),
            )
            .into());
        }

        spinner.finish_with_message(format!(
            "{} No downloadable files found in metadata",
            "✘".red().bold()
        ));
        return Err(IaGetError::Network(
            "No downloadable files found in archive metadata.".to_string(),
        )
        .into());
    }

    // Successfully finished initialization
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template(&format!(
                "{} {} to download {} files from archive.org {}",
                "✔".green().bold(),
                "Ready".bold(),
                download_data.len().to_string().bold(),
                "★".yellow()
            ))
            .expect("Failed to set completion style"),
    );
    spinner.finish();

    if is_authenticated {
        println!(
            "{} {}",
            "✓".green().bold(),
            "Authenticated archive.org session active".bold()
        );
    }

    // Warn user if filename was modified
    for (original_name, sanitized_name) in sanitized_files {
        println!(
            "{} {} {} → {}",
            "⚠".yellow().bold(),
            "Sanitized:".yellow(),
            original_name.dimmed(),
            sanitized_name.bold()
        );
    }

    if private_count > 0 {
        println!(
            "\n{} {} {} private/restricted file{} listed in metadata",
            "⚠".yellow().bold(),
            "Skipped".yellow().bold(),
            private_count.to_string().bold(),
            if private_count == 1 { "" } else { "s" }
        );
    }

    // Show summary if any files were sanitized
    if sanitized_count > 0 {
        println!(
            "\n{} {} {} file{} for filesystem compatibility",
            "✓".green().bold(),
            "Sanitized".bold(),
            sanitized_count.to_string().bold(),
            if sanitized_count == 1 { "" } else { "s" }
        );
    }

    // Download all files with integrated signal handling
    downloader::download_files(&client, download_data.clone(), download_data.len()).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;

    /// Sample success response of the XAuthn login endpoint
    const XAUTHN_SUCCESS_JSON: &str = r#"{
        "success": true,
        "version": 1,
        "values": {
            "screenname": "someuser",
            "s3": {"access": "abc", "secret": "xyz"},
            "cookies": {
                "logged-in-user": "user%40example.com",
                "logged-in-sig": "s1gn4tur3"
            }
        }
    }"#;
    use ia_get::utils::validate_archive_url;

    #[test]
    fn check_valid_pattern() {
        assert!(validate_archive_url("https://archive.org/details/Valid-Pattern").is_ok());
        assert!(validate_archive_url("https://archive.org/details/Valid-Pattern/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test123").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test123/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test_file-name.data").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test_file-name.data/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/user@domain").is_ok());
        assert!(validate_archive_url("https://archive.org/details/user@domain/").is_ok());
    }

    #[test]
    fn check_invalid_pattern() {
        assert!(validate_archive_url("https://archive.org/details/Invalid-Pattern-*").is_err());
        assert!(validate_archive_url("https://archive.org/details/").is_err()); // This should still be an error (empty identifier)
        assert!(validate_archive_url("https://example.com/details/test").is_err());
        assert!(validate_archive_url("http://archive.org/details/test").is_err());
        assert!(validate_archive_url("https://archive.org/details/test/extra").is_err());
        assert!(validate_archive_url("https://archive.org/details/test//").is_err());
        // Multiple trailing slashes
    }

    #[test]
    fn check_get_xml_url() {
        assert_eq!(
            get_xml_url("https://archive.org/details/item1"),
            "https://archive.org/download/item1/item1_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/item1/"), // With trailing slash
            "https://archive.org/download/item1/item1_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/another-item_v2.0"),
            "https://archive.org/download/another-item_v2.0/another-item_v2.0_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/another-item_v2.0/"), // With trailing slash
            "https://archive.org/download/another-item_v2.0/another-item_v2.0_files.xml"
        );
    }

    #[test]
    fn encode_archive_path_for_url_encodes_each_path_segment() {
        let path = "DATs/Bandai - WonderSwan Color [T-En] Collection (06-10-2022).zip";
        let encoded = encode_archive_path_for_url(path);
        assert_eq!(
            encoded,
            "DATs/Bandai%20-%20WonderSwan%20Color%20%5BT-En%5D%20Collection%20%2806-10-2022%29.zip"
        );
    }

    #[test]
    fn encode_archive_path_for_url_encodes_literal_percent() {
        let path = "100% Pascal Sensei - Kanpeki Paint Bombers (Japan).7z";
        let encoded = encode_archive_path_for_url(path);

        assert_eq!(
            encoded,
            "100%25%20Pascal%20Sensei%20-%20Kanpeki%20Paint%20Bombers%20%28Japan%29.7z"
        );
    }

    #[test]
    fn encoded_archive_path_joins_to_expected_download_url() {
        let base_url = reqwest::Url::parse(
            "https://archive.org/download/3ds-main-encrypted/3ds-main-encrypted_files.xml",
        )
        .expect("base URL should parse");
        let encoded =
            encode_archive_path_for_url("100% Pascal Sensei - Kanpeki Paint Bombers (Japan).7z");
        let download_url = base_url.join(&encoded).expect("encoded path should join");

        assert_eq!(
            download_url.as_str(),
            "https://archive.org/download/3ds-main-encrypted/100%25%20Pascal%20Sensei%20-%20Kanpeki%20Paint%20Bombers%20%28Japan%29.7z"
        );
    }

    #[test]
    fn trim_trailing_newlines_only() {
        assert_eq!(trim_trailing_newlines("secret\n".to_string()), "secret");
        assert_eq!(trim_trailing_newlines("secret\r\n".to_string()), "secret");
        assert_eq!(trim_trailing_newlines(" secret ".to_string()), " secret ");
    }

    #[test]
    fn resolve_auth_credentials_without_auth() {
        let cli = Cli {
            url: "https://archive.org/details/item1".to_string(),
            username: None,
            password: None,
            password_stdin: false,
        };

        let credentials = resolve_auth_credentials(&cli).expect("No auth should be valid");
        assert!(credentials.is_none());
    }

    #[test]
    fn resolve_auth_credentials_requires_password() {
        let cli = Cli {
            url: "https://archive.org/details/item1".to_string(),
            username: Some("user@example.com".to_string()),
            password: None,
            password_stdin: false,
        };

        let error = resolve_auth_credentials(&cli).expect_err("Missing password should fail");
        assert!(error
            .to_string()
            .contains("Authentication requires --password"));
    }

    #[test]
    fn resolve_auth_credentials_rejects_empty_username() {
        let cli = Cli {
            url: "https://archive.org/details/item1".to_string(),
            username: Some("   ".to_string()),
            password: Some("s3cret".to_string()),
            password_stdin: false,
        };

        let error = resolve_auth_credentials(&cli).expect_err("Empty username should fail");
        assert!(error.to_string().contains("username cannot be empty"));
    }

    #[test]
    fn resolve_auth_credentials_with_password() {
        let cli = Cli {
            url: "https://archive.org/details/item1".to_string(),
            username: Some("user@example.com".to_string()),
            password: Some("s3cret".to_string()),
            password_stdin: false,
        };

        let credentials = resolve_auth_credentials(&cli).expect("Credentials should resolve");
        assert_eq!(
            credentials,
            Some(("user@example.com".to_string(), "s3cret".to_string()))
        );
    }

    #[test]
    fn resolve_auth_credentials_rejects_empty_password() {
        let cli = Cli {
            url: "https://archive.org/details/item1".to_string(),
            username: Some("user@example.com".to_string()),
            password: Some("".to_string()),
            password_stdin: false,
        };

        let error = resolve_auth_credentials(&cli).expect_err("Empty password should fail");
        assert!(error.to_string().contains("password cannot be empty"));
    }

    #[test]
    fn xauthn_success_payload_yields_session_cookies() {
        let payload: XAuthnResponse =
            serde_json::from_str(XAUTHN_SUCCESS_JSON).expect("Sample XAuthn payload should parse");

        let session = parse_login_session(&payload).expect("Session cookies should be extracted");
        assert_eq!(session.logged_in_user, "user%40example.com");
        assert_eq!(session.logged_in_sig, "s1gn4tur3");

        for cookie in session.cookie_strings() {
            assert!(cookie.contains("logged-in-user=") || cookie.contains("logged-in-sig="));
            assert!(cookie.contains("Domain=.archive.org"));
            assert!(cookie.contains("Path=/"));
            assert!(cookie.contains("Secure"));
        }
    }

    #[test]
    fn xauthn_failure_reason_maps_to_actionable_message() {
        let payload: XAuthnResponse = serde_json::from_str(
            r#"{"success":false,"values":{"reason":"account_bad_password"},"version":1}"#,
        )
        .expect("Failure payload should parse");

        let description = describe_xauthn_failure(StatusCode::UNAUTHORIZED, &payload);
        assert!(description.contains("HTTP 401"));
        assert!(description.contains("Incorrect password"));
    }

    #[test]
    fn xauthn_unknown_failure_reason_is_shown_verbatim() {
        let payload: XAuthnResponse = serde_json::from_str(
            r#"{"success":false,"values":{"reason":"server_on_fire"},"version":1}"#,
        )
        .expect("Failure payload should parse");

        let description = describe_xauthn_failure(StatusCode::UNAUTHORIZED, &payload);
        assert!(description.contains("server_on_fire"));
    }

    #[test]
    fn xauthn_failure_without_reason_falls_back_to_error_field() {
        let payload: XAuthnResponse =
            serde_json::from_str(r#"{"success":false,"error":"bad request","version":1}"#)
                .expect("Failure payload should parse");

        let description = describe_xauthn_failure(StatusCode::BAD_REQUEST, &payload);
        assert!(description.contains("bad request"));
    }

    #[test]
    fn session_cookies_are_sent_to_archive_org_and_download_subdomains() {
        let session = LoginSession {
            logged_in_user: "user%40example.com".to_string(),
            logged_in_sig: "s1gn4tur3".to_string(),
        };
        let jar = Jar::default();
        let archive_org = reqwest::Url::parse("https://archive.org/").unwrap();

        for cookie in session.cookie_strings() {
            jar.add_cookie_str(&cookie, &archive_org);
        }

        for url in [
            "https://archive.org/download/some-item/file.zip",
            "https://ia601504.us.archive.org/some-item/file.zip",
        ] {
            let header = jar
                .cookies(&reqwest::Url::parse(url).unwrap())
                .unwrap_or_else(|| panic!("cookies should be sent to {url}"));
            let header = header.to_str().unwrap();
            assert!(
                header.contains("logged-in-user=user%40example.com"),
                "{url}"
            );
            assert!(header.contains("logged-in-sig=s1gn4tur3"), "{url}");
        }
    }

    #[test]
    fn parse_login_session_rejects_payload_without_cookies() {
        let payload: XAuthnResponse = serde_json::from_str(
            r#"{"success":true,"values":{"screenname":"someuser"},"version":1}"#,
        )
        .expect("Payload should parse");

        let error = parse_login_session(&payload).expect_err("Missing cookies should fail");
        assert!(error.to_string().contains("missing session cookies"));
    }
}
