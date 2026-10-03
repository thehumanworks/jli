//! The loopback HTTP listener the browser returns to after sign-in.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::error::{Error, Result, SignInError};

/// What the browser brought back to the callback.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Callback {
    /// The authorization code, on success.
    pub code: Option<String>,
    /// The state the authorization request sent.
    pub state: Option<String>,
    /// The client id issued by a new registration.
    pub client_id: Option<String>,
    /// The granted scopes, space-separated. The token response's scopes are the authority.
    pub scope: Option<String>,
    /// The OAuth error, such as `access_denied`.
    pub error: Option<String>,
    /// The error's description.
    pub error_description: Option<String>,
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("state", &self.state)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("error", &self.error)
            .field("error_description", &self.error_description)
            .finish()
    }
}

impl Callback {
    /// Read a callback's query string (without the `?`).
    pub fn from_query(query: &str) -> Callback {
        let mut callback = Callback::default();
        for (name, value) in super::form_decode(query) {
            let slot = match name.as_str() {
                "code" => &mut callback.code,
                "state" => &mut callback.state,
                "client_id" => &mut callback.client_id,
                "scope" => &mut callback.scope,
                "error" => &mut callback.error,
                "error_description" => &mut callback.error_description,
                _ => continue,
            };
            *slot = Some(value);
        }
        callback
    }

    /// Read the callback from a whole URL, such as one pasted from a browser on another machine.
    pub fn from_url(url: &str) -> Callback {
        let query = url.split_once('?').map(|(_, query)| query).unwrap_or("");
        Callback::from_query(query.split('#').next().unwrap_or(""))
    }
}

/// An HTTP listener on `127.0.0.1` that receives one sign-in callback.
///
/// Start it before opening the browser. The redirect URI is
/// `http://127.0.0.1:<port>/auth/callback`; only the port may change between sign-ins.
#[derive(Debug)]
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
    app_name: String,
}

impl LoopbackListener {
    /// The port tried first.
    pub const DEFAULT_PORT: u16 = 1455;
    /// The callback's path.
    pub const CALLBACK_PATH: &'static str = "/auth/callback";

    /// Listen on port 1455, or on any free port if that one is taken.
    pub fn bind() -> Result<LoopbackListener> {
        LoopbackListener::bind_port(LoopbackListener::DEFAULT_PORT).or_else(|_| LoopbackListener::bind_port(0))
    }

    /// Listen on `port`; 0 picks a free one.
    pub fn bind_port(port: u16) -> Result<LoopbackListener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|error| Error::io(format!("listening on 127.0.0.1:{port} for the sign-in callback"), error))?;
        let port = listener.local_addr().map_err(|error| Error::io("reading the callback port", error))?.port();
        Ok(LoopbackListener { listener, port, app_name: "the app".into() })
    }

    /// Name the app on the page the browser shows when it returns.
    pub fn app_name(mut self, name: impl Into<String>) -> LoopbackListener {
        self.app_name = name.into();
        self
    }

    /// The port in use.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI to send in the authorization request and the code exchange.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, LoopbackListener::CALLBACK_PATH)
    }

    /// Wait for the browser to return, and answer it with a page. A request for another path is
    /// answered 404, and a callback with another state 400, and both are ignored: a stray or forged
    /// request from another page cannot end the sign-in. The callback with this attempt's state is
    /// returned whatever else it says; check it with [`super::AuthorizationRequest::complete`].
    pub fn wait(&self, expected_state: &str, timeout: Duration) -> Result<Callback> {
        let deadline = Instant::now() + timeout;
        self.listener.set_nonblocking(true).map_err(|error| Error::io("waiting for the sign-in callback", error))?;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(callback) = self.answer(stream, expected_state) {
                        return Ok(callback);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(SignInError::TimedOut.into());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(Error::io("waiting for the sign-in callback", error)),
            }
        }
    }

    /// Read one request and answer it. `Some` when it was the callback.
    fn answer(&self, stream: TcpStream, expected_state: &str) -> Option<Callback> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).ok()?;
        // Drain the headers, so the browser sees a clean close.
        let mut header = String::new();
        while reader.read_line(&mut header).ok()? > 2 {
            header.clear();
        }
        let mut stream = reader.into_inner();
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != LoopbackListener::CALLBACK_PATH {
            respond(&mut stream, "404 Not Found", "Not found", "This is not the sign-in callback.");
            return None;
        }
        let callback = Callback::from_query(query);
        let app = escape(&self.app_name);
        if callback.state.as_deref() != Some(expected_state) {
            let text = format!("This sign-in does not belong to the one {app} is waiting for. Close this tab.");
            respond(&mut stream, "400 Bad Request", "Sign-in not recognised", &text);
            return None;
        } else if let Some(error) = &callback.error {
            let text = format!("Sign-in did not complete ({}). You can close this tab and return to {app}.", escape(error));
            respond(&mut stream, "200 OK", "Sign-in did not complete", &text);
        } else {
            respond(&mut stream, "200 OK", "Signed in", &format!("You can close this tab and return to {app}."));
        }
        Some(callback)
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn respond(stream: &mut TcpStream, status: &str, title: &str, text: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family: system-ui, sans-serif; margin: 4rem auto; max-width: 32rem\">\
         <h1>{title}</h1><p>{text}</p></body></html>"
    );
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // The browser may already have gone; the callback is what matters.
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn browse(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/html\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn the_callback_is_read_and_answered() {
        let listener = LoopbackListener::bind_port(0).unwrap().app_name("jli <test>");
        assert_eq!(listener.redirect_uri(), format!("http://127.0.0.1:{}/auth/callback", listener.port()));
        let port = listener.port();
        let browser = std::thread::spawn(move || {
            let favicon = browse(port, "/favicon.ico");
            let page = browse(port, "/auth/callback?code=c-1&state=s-1&client_id=oaiapp_1&scope=openid+email");
            (favicon, page)
        });
        let callback = listener.wait("s-1", Duration::from_secs(10)).unwrap();
        let (favicon, page) = browser.join().unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404"));
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("return to jli &lt;test&gt;"), "{page}");
        assert_eq!(callback.code.as_deref(), Some("c-1"));
        assert_eq!(callback.client_id.as_deref(), Some("oaiapp_1"));
        assert_eq!(callback.scope.as_deref(), Some("openid email"));
        assert!(!format!("{callback:?}").contains("c-1"), "the code is not logged");
    }

    #[test]
    fn a_stranger_is_turned_away_and_an_error_gets_its_own_page() {
        let listener = LoopbackListener::bind_port(0).unwrap();
        let port = listener.port();
        let browser = std::thread::spawn(move || {
            let forged = browse(port, "/auth/callback?state=forged&code=x");
            (forged, browse(port, "/auth/callback?state=s-1&code=real"))
        });
        let callback = listener.wait("s-1", Duration::from_secs(10)).unwrap();
        let (forged, real) = browser.join().unwrap();
        assert!(forged.starts_with("HTTP/1.1 400") && real.starts_with("HTTP/1.1 200"));
        assert_eq!(callback.code.as_deref(), Some("real"), "the forged callback did not end the wait");

        let browser = std::thread::spawn(move || browse(port, "/auth/callback?state=s-1&error=access_denied"));
        let callback = listener.wait("s-1", Duration::from_secs(10)).unwrap();
        assert!(browser.join().unwrap().contains("access_denied"));
        assert_eq!(callback.error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn waiting_ends_at_the_deadline() {
        let listener = LoopbackListener::bind_port(0).unwrap();
        let error = listener.wait("s", Duration::from_millis(120)).unwrap_err();
        assert!(matches!(error, Error::SignIn(SignInError::TimedOut)));
    }

    #[test]
    fn a_pasted_url_is_read() {
        let callback = Callback::from_url("http://127.0.0.1:1455/auth/callback?code=a&state=b#fragment");
        assert_eq!((callback.code.as_deref(), callback.state.as_deref()), (Some("a"), Some("b")));
        assert_eq!(Callback::from_url("no query"), Callback::default());
    }
}
