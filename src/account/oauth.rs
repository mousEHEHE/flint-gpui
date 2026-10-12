//! GitHub sign-in in the user's browser. GitHub sends the browser back to a
//! port on this machine that is open only while the sign-in runs.

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use base64::Engine as _;
use rand::{rngs::OsRng, RngCore as _};
use sha2::{Digest as _, Sha256};

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("sign-in was cancelled")]
    Cancelled,
    #[error("GitHub sign-in was not finished in time")]
    TimedOut,
    #[error("GitHub sign-in was declined")]
    Declined,
    #[error("could not wait for the browser: {0}")]
    Io(#[from] std::io::Error),
}

/// What the server needs to finish the sign-in with GitHub.
pub struct Callback {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

/// Run the browser part of a sign-in. `open_browser` gets the page to show.
/// Blocks until GitHub sends the browser back, `is_cancelled` is set, or
/// five minutes pass.
pub fn authorize(
    client_id: &str,
    open_browser: impl FnOnce(String),
    is_cancelled: &AtomicBool,
) -> Result<Callback, OAuthError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );

    let code_verifier = random_token();
    let state = random_token();
    let challenge = BASE64_URL.encode(Sha256::digest(code_verifier.as_bytes()));
    open_browser(format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&state={state}\
         &code_challenge={challenge}&code_challenge_method=S256",
        percent_encode(client_id),
        percent_encode(&redirect_uri),
    ));

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        if is_cancelled.load(Ordering::Relaxed) {
            return Err(OAuthError::Cancelled);
        }
        if Instant::now() > deadline {
            return Err(OAuthError::TimedOut);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = answer(stream, &state)? {
                    return Ok(Callback {
                        code,
                        code_verifier,
                        redirect_uri,
                    });
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Answer one browser request. `Some(code)` when it was GitHub's redirect.
fn answer(mut stream: TcpStream, state: &str) -> Result<Option<String>, OAuthError> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut buffer = [0u8; 4096];
    // A request that does not arrive is not worth failing the sign-in for.
    let length = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..length]);
    let target = request.split_whitespace().nth(1).unwrap_or("");

    let Some(query) = target.strip_prefix("/callback?") else {
        respond(&mut stream, "404 Not Found", "Not found.");
        return Ok(None);
    };
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    };

    // Another page on this machine cannot know the state, so its request is ignored.
    if param("state") != Some(state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "This sign-in link is not valid.",
        );
        return Ok(None);
    }
    if param("error").is_some() {
        respond(
            &mut stream,
            "200 OK",
            "Sign-in was declined. You can close this tab.",
        );
        return Err(OAuthError::Declined);
    }
    let Some(code) = param("code") else {
        respond(
            &mut stream,
            "400 Bad Request",
            "This sign-in link is not valid.",
        );
        return Ok(None);
    };
    respond(
        &mut stream,
        "200 OK",
        "Signed in. You can close this tab and return to flint.",
    );
    Ok(Some(code.to_string()))
}

fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let page = format!(
        "<!doctype html><meta charset=utf-8><title>flint</title>\
         <body style=\"font:16px system-ui;margin:20vh auto;max-width:28em;text-align:center\">\
         <p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    // The browser may have gone away; the sign-in does not depend on this page.
    let _ = stream.write_all(response.as_bytes());
}

/// 32 random bytes as 43 URL-safe characters.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    BASE64_URL.encode(bytes)
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str, path: &str) -> String {
        let address = url
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// Runs `authorize` with a "browser" that calls back with `reply(state)`.
    fn sign_in(
        reply: impl FnOnce(&str) -> String + Send + 'static,
    ) -> Result<Callback, OAuthError> {
        let is_cancelled = AtomicBool::new(false);
        authorize(
            "client",
            |url| {
                let redirect = url
                    .split("redirect_uri=")
                    .nth(1)
                    .unwrap()
                    .split('&')
                    .next()
                    .unwrap();
                let redirect = redirect.replace("%3A", ":").replace("%2F", "/");
                let state = url
                    .split("state=")
                    .nth(1)
                    .unwrap()
                    .split('&')
                    .next()
                    .unwrap()
                    .to_string();
                std::thread::spawn(move || {
                    // Noise first: the sign-in must wait for the real redirect.
                    get(&redirect, "/favicon.ico");
                    get(&redirect, "/callback?code=stolen&state=guess");
                    get(&redirect, &reply(&state));
                });
            },
            &is_cancelled,
        )
    }

    #[test]
    fn the_code_from_the_redirect_is_returned() {
        let callback = sign_in(|state| format!("/callback?code=abc123&state={state}")).unwrap();
        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.code_verifier.len(), 43);
        assert!(callback.redirect_uri.starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn a_declined_sign_in_is_reported() {
        let result = sign_in(|state| format!("/callback?error=access_denied&state={state}"));
        assert!(matches!(result, Err(OAuthError::Declined)));
    }

    #[test]
    fn a_cancelled_sign_in_stops_waiting() {
        let is_cancelled = AtomicBool::new(true);
        let result = authorize("client", |_| {}, &is_cancelled);
        assert!(matches!(result, Err(OAuthError::Cancelled)));
    }

    #[test]
    fn the_redirect_address_is_encoded_for_the_query() {
        assert_eq!(
            percent_encode("http://127.0.0.1:80/cb"),
            "http%3A%2F%2F127.0.0.1%3A80%2Fcb"
        );
    }
}
