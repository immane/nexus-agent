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
        "/assets/manifest.webmanifest" => Some((
            "application/manifest+json; charset=utf-8",
            include_bytes!("../web/manifest.webmanifest"),
        )),
        "/assets/icon-192.png" => Some(("image/png", include_bytes!("../web/icon-192.png"))),
        "/assets/icon-512.png" => Some(("image/png", include_bytes!("../web/icon-512.png"))),
        "/sw.js" => Some((
            "text/javascript; charset=utf-8",
            include_bytes!("../web/sw.js"),
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
    "style-src 'self'; img-src 'self' data:; connect-src 'self'; ",
    "manifest-src 'self'; worker-src 'self'; base-uri 'none'; ",
    "form-action 'none'; frame-ancestors 'none'\r\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_fixed_assets_are_served() {
        for path in ["/", "/assets/app.css", "/assets/app.js", "/sw.js"] {
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
    fn install_assets_are_well_formed() {
        let (mime, body) = asset("/assets/manifest.webmanifest").expect("manifest resolves");
        assert_eq!(mime, "application/manifest+json; charset=utf-8");
        let manifest: serde_json::Value = serde_json::from_slice(body).expect("manifest JSON");
        assert_eq!(manifest["name"], "Nexus Agent");
        assert_eq!(manifest["start_url"], "/");
        assert_eq!(manifest["scope"], "/");
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["icons"].as_array().expect("icons").len(), 2);

        for (path, size) in [("/assets/icon-192.png", 192), ("/assets/icon-512.png", 512)] {
            let (mime, body) = asset(path).expect("icon resolves");
            assert_eq!(mime, "image/png");
            assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(
                u32::from_be_bytes(body[16..20].try_into().expect("width")),
                size
            );
            assert_eq!(
                u32::from_be_bytes(body[20..24].try_into().expect("height")),
                size
            );
        }

        let (_, worker) = asset("/sw.js").expect("service worker resolves");
        let worker = std::str::from_utf8(worker).expect("worker is text");
        assert!(worker.contains("addEventListener('fetch'"));
        assert!(worker.contains("skipWaiting"));
        assert!(
            !worker.contains("caches.open"),
            "the worker must not cache API traffic"
        );
    }

    #[test]
    fn browser_security_headers_are_explicit() {
        assert!(HEADERS.contains("frame-ancestors 'none'"));
        assert!(HEADERS.contains("x-frame-options: DENY"));
        assert!(HEADERS.contains("default-src 'none'"));
        assert!(HEADERS.contains("manifest-src 'self'"));
        assert!(HEADERS.contains("worker-src 'self'"));
        assert!(HEADERS.contains("x-content-type-options: nosniff"));
        assert!(HEADERS.contains("cache-control: no-cache"));
    }
}
