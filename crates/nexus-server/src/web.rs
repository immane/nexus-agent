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
    "x-frame-options: DENY\r\n",
    "referrer-policy: no-referrer\r\n",
    "content-security-policy: default-src 'none'; script-src 'self'; ",
    "style-src 'self'; img-src data:; connect-src 'self'; base-uri 'none'; ",
    "form-action 'none'; frame-ancestors 'none'\r\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_fixed_assets_are_served() {
        for path in ["/", "/assets/app.css", "/assets/app.js"] {
            let (mime, body) = asset(path).expect("documented asset resolves");
            assert!(mime.starts_with("text/"));
            assert!(!body.is_empty());
        }
        for path in [
            "",
            "/index.html",
            "/assets",
            "/assets/",
            "/assets/missing.js",
            "/assets/../app.js",
            "/assets/app.js/extra",
        ] {
            assert!(asset(path).is_none(), "unexpected asset: {path}");
        }
    }

    #[test]
    fn browser_security_headers_are_explicit() {
        assert!(HEADERS.contains("frame-ancestors 'none'"));
        assert!(HEADERS.contains("x-frame-options: DENY"));
        assert!(HEADERS.contains("default-src 'none'"));
        assert!(HEADERS.contains("x-content-type-options: nosniff"));
        assert!(HEADERS.contains("cache-control: no-cache"));
    }
}
