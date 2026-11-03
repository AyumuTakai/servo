/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The error page shown in place of a document that could not be loaded, with the information
//! other browsers show: the kind of failure (name not resolved, connection refused, timed out, ...),
//! the HTTP status, the URL, and a reload button. The text is in `neterror.html`.

use net_traits::NetworkError;

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The kind of failure (a key of the table in `neterror.html`) and the error name that Chrome shows.
fn classify(error: &NetworkError) -> (&'static str, &'static str) {
    match error {
        NetworkError::HttpError(reason) |
        NetworkError::ResourceLoadError(reason) |
        NetworkError::WebsocketConnectionFailure(reason) => {
            let reason = reason.to_lowercase();
            let has = |needles: &[&str]| needles.iter().any(|needle| reason.contains(needle));
            if has(&[
                "dns error",
                "failed to lookup",
                "name or service not known",
                "nodename nor servname",
                "no such host",
                "no address associated",
            ]) {
                ("name_not_resolved", "ERR_NAME_NOT_RESOLVED")
            } else if has(&["connection refused"]) {
                ("connection_refused", "ERR_CONNECTION_REFUSED")
            } else if has(&["timed out", "timeout"]) {
                ("timed_out", "ERR_CONNECTION_TIMED_OUT")
            } else if has(&["connection reset", "broken pipe"]) {
                ("connection_reset", "ERR_CONNECTION_RESET")
            } else if has(&["network is unreachable", "no route to host", "host is unreachable"]) {
                ("unreachable", "ERR_ADDRESS_UNREACHABLE")
            } else if has(&["connect"]) {
                ("connection_failed", "ERR_CONNECTION_FAILED")
            } else {
                ("failed", "ERR_FAILED")
            }
        },
        NetworkError::UnsupportedScheme => ("unsupported_scheme", "ERR_UNKNOWN_URL_SCHEME"),
        NetworkError::TooManyRedirects | NetworkError::RedirectError => {
            ("redirects", "ERR_TOO_MANY_REDIRECTS")
        },
        NetworkError::ContentSecurityPolicy |
        NetworkError::MixedContent |
        NetworkError::Nosniff => ("blocked", "ERR_BLOCKED_BY_CLIENT"),
        NetworkError::InvalidPort => ("blocked", "ERR_UNSAFE_PORT"),
        _ => ("failed", "ERR_FAILED"),
    }
}

fn render(template: &str, kind: &str, code: &str, status: u16, url: &str, host: &str, detail: &str) -> String {
    template
        .replace("${kind}", kind)
        .replace("${code}", &escape_html(code))
        .replace("${status}", &status.to_string())
        .replace("${host}", &escape_html(host))
        .replace("${url}", &escape_html(url))
        .replace("${detail}", &escape_html(detail))
}

/// The page for a failed network request.
pub(super) fn network_error_page(
    template: &str,
    url: &str,
    host: Option<&str>,
    error: &NetworkError,
) -> String {
    let (kind, code) = classify(error);
    render(template, kind, code, 0, url, host.unwrap_or(""), &format!("{error:?}"))
}

/// The page for a main-frame response with an HTTP error status and no body (as Chrome and Firefox
/// show their own page for these instead of a blank page).
pub(super) fn http_status_page(template: &str, url: &str, host: Option<&str>, status: u16) -> String {
    let kind = match status {
        404 | 410 => "not_found",
        500..=599 => "server_error",
        _ => "http_error",
    };
    let phrase = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("");
    render(
        template,
        kind,
        &format!("HTTP ERROR {status}"),
        status,
        url,
        host.unwrap_or(""),
        &format!("{status} {phrase}"),
    )
}
