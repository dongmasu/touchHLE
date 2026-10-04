/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use super::{unix_seconds, AuthError, TokenSet, GOOGLE_DRIVE_SCOPE};
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(300);

fn loopback_redirect_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub struct PendingAuthorization {
    receiver: Receiver<Result<TokenSet, AuthError>>,
}

impl PendingAuthorization {
    pub fn try_result(&self) -> Result<Option<Result<TokenSet, AuthError>>, AuthError> {
        match self.receiver.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(AuthError::OAuth(
                "authorization worker stopped unexpectedly".into(),
            )),
        }
    }
}

pub fn start_authorization(
    client_id: String,
    client_secret: Option<String>,
    open_browser: impl FnOnce(&str) -> Result<(), String>,
) -> Result<PendingAuthorization, AuthError> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .map_err(|_| AuthError::OAuth("could not start the local authorization callback".into()))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| AuthError::OAuth("could not configure the authorization callback".into()))?;
    let redirect_url = loopback_redirect_url(
        listener
            .local_addr()
            .map_err(|_| AuthError::OAuth("could not read the local callback address".into()))?
            .port(),
    );
    let client = BasicClient::new(ClientId::new(client_id))
        .set_auth_uri(
            AuthUrl::new(GOOGLE_AUTH_URL.to_owned())
                .map_err(|_| AuthError::OAuth("invalid Google authorization endpoint".into()))?,
        )
        .set_token_uri(
            TokenUrl::new(GOOGLE_TOKEN_URL.to_owned())
                .map_err(|_| AuthError::OAuth("invalid Google token endpoint".into()))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_url)
                .map_err(|_| AuthError::OAuth("invalid local callback address".into()))?,
        );
    let client_secret_configured = client_secret.is_some();
    let client = if let Some(client_secret) = client_secret {
        client.set_client_secret(ClientSecret::new(client_secret))
    } else {
        client
    };
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (authorization_url, expected_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(GOOGLE_DRIVE_SCOPE.to_owned()))
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(challenge)
        .url();

    open_browser(authorization_url.as_str()).map_err(AuthError::OAuth)?;

    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("touchHLE-google-oauth".to_owned())
        .spawn(move || {
            let result =
                receive_authorization_code(listener, expected_state.secret()).and_then(|code| {
                    let http_client = oauth2::reqwest::blocking::ClientBuilder::new()
                        .redirect(oauth2::reqwest::redirect::Policy::none())
                        .build()
                        .map_err(|_| {
                            AuthError::OAuth("could not initialize the HTTPS client".into())
                        })?;
                    log!(
                        "Google desktop OAuth token request fields: client_id, code, code_verifier, grant_type, redirect_uri; client_secret configured: {}.",
                        if client_secret_configured { "yes" } else { "no" }
                    );
                    let response = client
                        .exchange_code(code)
                        .set_pkce_verifier(verifier)
                        .request(&http_client)
                        .map_err(|error| AuthError::OAuth(token_exchange_error_message(error)))?;
                    let refresh_token = response
                        .refresh_token()
                        .ok_or(AuthError::InvalidResponse)?
                        .secret()
                        .to_owned();
                    let expires_in = response
                        .expires_in()
                        .map_or(3600, |duration| duration.as_secs());
                    Ok(TokenSet::new(
                        response.access_token().secret().to_owned(),
                        refresh_token,
                        unix_seconds().saturating_add(expires_in),
                    ))
                });
            if let Err(error) = &result {
                let diagnostic = match error {
                    AuthError::OAuth(message) => message.as_str(),
                    AuthError::InvalidResponse => "Google returned an incomplete token response",
                    AuthError::Cancelled => "authorization was cancelled",
                    _ => "unexpected authorization failure",
                };
                log!("Google desktop authorization failed: {diagnostic}");
            }
            let _ = sender.send(result);
        })
        .map_err(|_| AuthError::OAuth("could not start the authorization worker".into()))?;
    Ok(PendingAuthorization { receiver })
}

fn token_exchange_error_message<E>(
    error: oauth2::RequestTokenError<E, oauth2::basic::BasicErrorResponse>,
) -> String
where
    E: std::error::Error + 'static,
{
    match error {
        oauth2::RequestTokenError::ServerResponse(response) => {
            let code = match response.error() {
                oauth2::basic::BasicErrorResponseType::InvalidClient => "invalid_client",
                oauth2::basic::BasicErrorResponseType::InvalidGrant => "invalid_grant",
                oauth2::basic::BasicErrorResponseType::InvalidRequest => "invalid_request",
                oauth2::basic::BasicErrorResponseType::InvalidScope => "invalid_scope",
                oauth2::basic::BasicErrorResponseType::UnauthorizedClient => "unauthorized_client",
                oauth2::basic::BasicErrorResponseType::UnsupportedGrantType => {
                    "unsupported_grant_type"
                }
                oauth2::basic::BasicErrorResponseType::Extension(_) => "other",
            };
            match classify_error_description(response.error_description().map(String::as_str)) {
                Some(detail) => {
                    format!("Google rejected token exchange ({code}; {detail})")
                }
                None => format!("Google rejected token exchange ({code})"),
            }
        }
        oauth2::RequestTokenError::Request(_) => "request to Google token endpoint failed".into(),
        oauth2::RequestTokenError::Parse(_, _) => {
            "Google returned an unreadable token response".into()
        }
        oauth2::RequestTokenError::Other(_) => {
            "Google returned an unexpected token-exchange error".into()
        }
    }
}

fn classify_error_description(description: Option<&str>) -> Option<&'static str> {
    let description = description?.to_ascii_lowercase();
    let parameter = [
        ("redirect_uri", "redirect_uri"),
        ("redirect uri", "redirect_uri"),
        ("client_id", "client_id"),
        ("client id", "client_id"),
        ("client_secret", "client_secret"),
        ("client secret", "client_secret"),
        ("code_verifier", "code_verifier"),
        ("code verifier", "code_verifier"),
        ("grant_type", "grant_type"),
        ("grant type", "grant_type"),
        ("authorization code", "authorization_code"),
        ("invalid_grant", "authorization_code"),
    ]
    .into_iter()
    .find_map(|(needle, name)| description.contains(needle).then_some(name));

    let Some(parameter) = parameter else {
        return Some("server detail omitted");
    };
    let is_missing = description.contains("missing") || description.contains("required");
    let detail = match (parameter, is_missing) {
        ("authorization_code", _) => "server detail points to authorization_code",
        ("redirect_uri", true) => "missing or invalid parameter: redirect_uri",
        ("client_id", true) => "missing or invalid parameter: client_id",
        ("client_secret", true) => "missing or invalid parameter: client_secret",
        ("code_verifier", true) => "missing or invalid parameter: code_verifier",
        ("grant_type", true) => "missing or invalid parameter: grant_type",
        ("redirect_uri", false) => "server detail points to redirect_uri",
        ("client_id", false) => "server detail points to client_id",
        ("client_secret", false) => "server detail points to client_secret",
        ("code_verifier", false) => "server detail points to code_verifier",
        ("grant_type", false) => "server detail points to grant_type",
        (_, _) => "server detail points to a known parameter",
    };
    Some(detail)
}

fn receive_authorization_code(
    listener: TcpListener,
    expected_state: &str,
) -> Result<AuthorizationCode, AuthError> {
    let started = Instant::now();
    while started.elapsed() < REDIRECT_TIMEOUT {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    send_response(
                        &mut stream,
                        403,
                        "This authorization callback is local only.",
                    );
                    continue;
                }
                match parse_callback(&mut stream, expected_state) {
                    Ok(code) => return Ok(code),
                    Err(AuthError::Cancelled) => return Err(AuthError::Cancelled),
                    Err(_) => {
                        send_response(&mut stream, 400, "Invalid authorization response. Retry.");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                return Err(AuthError::OAuth(
                    "could not receive the local authorization callback".into(),
                ));
            }
        }
    }
    Err(AuthError::OAuth(
        "authorization timed out before Google returned".into(),
    ))
}

fn parse_callback(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<AuthorizationCode, AuthError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| AuthError::OAuth("could not configure the local callback".into()))?;
    let mut request = [0u8; 8192];
    let length = stream
        .read(&mut request)
        .map_err(|_| AuthError::OAuth("could not read the local callback".into()))?;
    let request = std::str::from_utf8(&request[..length])
        .map_err(|_| AuthError::OAuth("Google returned an invalid callback".into()))?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| AuthError::OAuth("Google returned an invalid callback".into()))?;
    let callback = Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| AuthError::OAuth("Google returned an invalid callback".into()))?;
    if callback.path() != "/" {
        return Err(AuthError::OAuth("unexpected local callback path".into()));
    }
    let parameters: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
    if parameters.get("error").is_some() {
        send_response(
            stream,
            200,
            "Authorization cancelled. You may close this window.",
        );
        return Err(AuthError::Cancelled);
    }
    let state = parameters
        .get("state")
        .ok_or_else(|| AuthError::OAuth("Google omitted authorization state".into()))?;
    if state != expected_state {
        return Err(AuthError::OAuth("authorization state did not match".into()));
    }
    let code = parameters
        .get("code")
        .ok_or_else(|| AuthError::OAuth("Google omitted the authorization code".into()))?;
    send_response(
        stream,
        200,
        "Google Drive is connected. You may close this window.",
    );
    Ok(AuthorizationCode::new(code.clone()))
}

fn send_response(stream: &mut TcpStream, status: u16, message: &str) {
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>touchHLE</title><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Bad Request" },
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_redirect_uses_ip_and_port_without_a_path() {
        assert_eq!(loopback_redirect_url(54321), "http://127.0.0.1:54321");
    }

    #[test]
    fn token_exchange_diagnostic_keeps_error_code_but_discards_server_details() {
        let response = oauth2::basic::BasicErrorResponse::new(
            oauth2::basic::BasicErrorResponseType::InvalidGrant,
            Some("sensitive response details".into()),
            Some("https://example.invalid/sensitive-details".into()),
        );
        let diagnostic = token_exchange_error_message::<std::io::Error>(
            oauth2::RequestTokenError::ServerResponse(response),
        );

        assert_eq!(
            diagnostic,
            "Google rejected token exchange (invalid_grant; server detail omitted)"
        );
        assert!(!diagnostic.contains("sensitive"));
    }

    #[test]
    fn token_exchange_diagnostic_classifies_only_allowlisted_parameter_names() {
        let response = oauth2::basic::BasicErrorResponse::new(
            oauth2::basic::BasicErrorResponseType::InvalidRequest,
            Some("Missing required parameter: redirect_uri; user@example.com".into()),
            Some("https://example.invalid/private".into()),
        );
        let diagnostic = token_exchange_error_message::<std::io::Error>(
            oauth2::RequestTokenError::ServerResponse(response),
        );

        assert_eq!(
            diagnostic,
            "Google rejected token exchange (invalid_request; missing or invalid parameter: redirect_uri)"
        );
        assert!(!diagnostic.contains("user@example.com"));
        assert!(!diagnostic.contains("example.invalid"));
    }

    #[test]
    fn token_exchange_diagnostic_omits_unrecognized_server_text() {
        let response = oauth2::basic::BasicErrorResponse::new(
            oauth2::basic::BasicErrorResponseType::InvalidRequest,
            Some("Unexpected details for user@example.com".into()),
            None,
        );
        let diagnostic = token_exchange_error_message::<std::io::Error>(
            oauth2::RequestTokenError::ServerResponse(response),
        );

        assert_eq!(
            diagnostic,
            "Google rejected token exchange (invalid_request; server detail omitted)"
        );
        assert!(!diagnostic.contains("user@example.com"));
    }

    #[test]
    fn callback_requires_expected_state_and_returns_only_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            parse_callback(&mut stream, "expected")
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(
                b"GET /?code=authorization-code&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap().secret(),
            "authorization-code"
        );
    }

    #[test]
    fn callback_rejects_state_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            parse_callback(&mut stream, "expected")
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(
                b"GET /?code=authorization-code&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .unwrap();
        assert!(worker.join().unwrap().is_err());
    }
}
