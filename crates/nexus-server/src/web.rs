//! Fixed, embedded assets only: never resolve browser paths on the filesystem.

use crate::http::Response;

pub(crate) fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/" => Some((
            "text/html; charset=utf-8",
            include_bytes!("../web/index.html"),
        )),
        "/assets/app.css" => Some(("text/css; charset=utf-8", include_bytes!("../web/app.css"))),
        "/assets/app.js" => Some((
            "text/javascript; charset=utf-8",
            include_bytes!("../web/app.js"),
        )),
        _ => None,
    }
}

pub(crate) fn response(path: &str) -> Option<Response> {
    asset(path).map(|(mime, body)| Response::new(200, mime, body.to_vec()))
}

pub(crate) const HEADERS: &str = concat!(
    "cache-control: no-cache\r\n",
    "x-content-type-options: nosniff\r\n",
    "referrer-policy: no-referrer\r\n",
    "content-security-policy: default-src 'none'; script-src 'self'; ",
    "style-src 'self'; img-src data:; connect-src 'self'; base-uri 'none'; ",
    "form-action 'none'; frame-ancestors 'none'\r\n",
);
