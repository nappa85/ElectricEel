//! Shared-destination parsing for navigation share (see
//! `docs/navigation-share.md`). Transport-agnostic: turns arbitrary
//! shared text (Sailfish Share, clipboard paste, URL open) into either exact
//! coordinates or an address string the car's nav resolves.
//!
//! This is the AUTHORITATIVE parser. The Go session child does NOT re-parse
//! URLs: it receives pre-parsed `navigate` args from `Core` (kind + values),
//! re-validates only shapes and ranges (defense in depth), and executes the
//! matching signed BLE action. QML never parses either: it calls
//! `core_preview_destination` to show what will be sent. Keep it that way
//! — one parser, no drift.

use std::fmt;

/// A parsed share destination.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Destination {
    /// Exact coordinates. Sent as a field-53 BLE `NavigationGpsRequest`.
    LatLon { lat: f64, lon: f64 },
    /// Address / place text / opaque URL. Sent as a field-21 BLE
    /// `NavigationRequest` destination string for the car to resolve.
    Address(String),
}

/// Why shared text was rejected. Surfaced to the UI verbatim-adjacent, so
/// keep messages human-readable and free of input echo (the input can be a
/// 2000-char URL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShareParseError {
    Empty,
    TooLong,
    OutOfRange,
}

impl fmt::Display for ShareParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareParseError::Empty => write!(f, "nothing to share: the text is empty"),
            ShareParseError::TooLong => write!(f, "shared text is too long (max 2000 characters)"),
            ShareParseError::OutOfRange => {
                write!(f, "coordinates out of range (lat -90..90, lon -180..180)")
            }
        }
    }
}

impl std::error::Error for ShareParseError {}

/// Maximum shared-text length in characters. Generous: a Google Maps share
/// URL with place name fits in a few hundred; anything beyond is a paste
/// accident.
pub(crate) const MAX_SHARE_TEXT_LEN: usize = 2000;

/// Parse arbitrary shared text into a [`Destination`].
///
/// Priority: embedded map URL (first `http(s)://` token) > `geo:` URI >
/// bare `lat,lon` pair > address text (first non-empty line). See the spike
/// doc §5 for the contract vectors.
pub(crate) fn parse_shared_text(input: &str) -> Result<Destination, ShareParseError> {
    if input.chars().count() > MAX_SHARE_TEXT_LEN {
        return Err(ShareParseError::TooLong);
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ShareParseError::Empty);
    }

    // A maps URL anywhere in the text (Android shares often come as
    // "Name\n\nhttps://...") beats everything else: coordinates hidden in
    // it are exact, and a bare address line next to it is redundant.
    if let Some(url) = first_url_token(trimmed) {
        if let Some(dest) = parse_map_url(&url) {
            return Ok(dest);
        }
        // Opaque URL (e.g. share.here.com): the car gets the URL itself.
        // Only fall through to geo:/bare/address when there is other text;
        // a lone URL is still a valid (if opaque) share.
        if first_non_empty_line(trimmed) == Some(url.as_str()) {
            return Ok(Destination::Address(url));
        }
    }

    let first_line = first_non_empty_line(trimmed).unwrap_or(trimmed);

    // geo: URI (RFC 5870), e.g. from Sailfish Maps / OSM share.
    if let Some(rest) = strip_prefix_case_insensitive(first_line, "geo:") {
        if let Some(dest) = parse_geo_uri(rest)? {
            return Ok(dest);
        }
        // parse_geo_uri returns Ok(None) only when coords are 0,0 with no
        // usable query — fall through to address handling below.
    }

    // Bare coordinate pair pasted from any app.
    if let Some((lat, lon)) = parse_latlon_pair(first_line) {
        return Ok(Destination::LatLon { lat, lon });
    }
    // A strict pair parse fails on "999,999" (out of range) the same way it
    // fails on "1600 Amphitheatre..." (not numeric). Distinguish them so a
    // coordinate-looking input gets a useful error instead of being routed
    // to the wrong continent as an "address".
    if looks_like_latlon_pair(first_line) {
        return Err(ShareParseError::OutOfRange);
    }

    Ok(Destination::Address(first_line.to_string()))
}

/// `Ok(Some)` = parsed, `Ok(None)` = `geo:0,0` without a query (caller falls
/// through to address handling), `Err` = coordinate-looking but out of range.
fn parse_geo_uri(rest: &str) -> Result<Option<Destination>, ShareParseError> {
    let (coords_part, query_part) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    // Strip an optional trailing `;u=` uncertainty parameter.
    let coords_part = match coords_part.find(';') {
        Some(i) => &coords_part[..i],
        None => coords_part,
    };
    // Query may carry the real destination when coords are 0,0.
    let query_addr = query_part
        .and_then(|q| query_param(q, "q"))
        .map(|s| decode_query_value(&s))
        .filter(|s| !s.trim().is_empty());

    if let Some((lat, lon)) = parse_latlon_pair(coords_part.trim()) {
        if lat == 0.0 && lon == 0.0 {
            // `geo:0,0?q=...` is an address search, not a point in the
            // Gulf of Guinea.
            if let Some(addr) = query_addr {
                return Ok(Some(Destination::Address(addr)));
            }
            return Ok(None);
        }
        return Ok(Some(Destination::LatLon { lat, lon }));
    }
    if looks_like_latlon_pair(coords_part.trim()) {
        return Err(ShareParseError::OutOfRange);
    }
    if let Some(addr) = query_addr {
        return Ok(Some(Destination::Address(addr)));
    }
    Ok(None)
}

/// Try a URL from a share payload. Returns `None` when the URL is opaque to
/// us (caller sends it as-is); never errors — an unparseable URL is still
/// shareable text.
fn parse_map_url(url: &str) -> Option<Destination> {
    let lower = url.to_ascii_lowercase();
    let host = url_host(&lower)?;

    if host.contains("google.") || host.contains("goo.gl") || host.contains("maps.app.goo.gl") {
        // ?q= / ?query= / ?daddr= : address or "lat,lon".
        for key in ["q", "query", "daddr"] {
            if let Some(raw) = url_query_param(url, key) {
                let val = decode_query_value(&raw);
                if val.trim().is_empty() {
                    continue;
                }
                if let Some((lat, lon)) = parse_latlon_pair(&val) {
                    return Some(Destination::LatLon { lat, lon });
                }
                // A bare `q=Eiffel+Tower` is an address; a full maps URL as
                // `q` value would recurse pointlessly — keep the decoded text.
                return Some(Destination::Address(val.trim().to_string()));
            }
        }
        // Embedded !3dLAT!4dLON markers are the most specific (the
        // destination); /@lat,lon earlier in the URL is the viewport.
        if let Some((lat, lon)) = google_3d4d_coords(url) {
            return Some(Destination::LatLon { lat, lon });
        }
        // /@lat,lon,... and /place/.../@lat,lon,...
        if let Some((lat, lon)) = url_path_at_coords(url) {
            return Some(Destination::LatLon { lat, lon });
        }
        return None;
    }

    if host.contains("maps.apple.com") {
        if let Some(raw) = url_query_param(url, "ll") {
            let val = decode_query_value(&raw);
            if let Some((lat, lon)) = parse_latlon_pair(&val) {
                return Some(Destination::LatLon { lat, lon });
            }
        }
        if let Some(raw) = url_query_param(url, "q") {
            let val = decode_query_value(&raw).trim().to_string();
            if val.is_empty() {
                return None;
            }
            if let Some((lat, lon)) = parse_latlon_pair(&val) {
                return Some(Destination::LatLon { lat, lon });
            }
            return Some(Destination::Address(val));
        }
        return None;
    }

    if host.contains("openstreetmap.org") {
        // Fragment #map=z/lat/lon.
        if let Some(frag) = url.split('#').nth(1) {
            let frag = frag.split('?').next().unwrap_or(frag);
            let parts: Vec<&str> = frag.split('/').collect();
            // "map=17/48.8584/2.2945" (3 parts, zoom glued to "map=")
            // or a bare "z/lat/lon" shape.
            let (lat_s, lon_s) = if parts.len() == 3 && parts[0].starts_with("map=") {
                (parts[1], parts[2])
            } else if parts.len() == 4 && parts[0] == "map" {
                (parts[2], parts[3])
            } else {
                ("", "")
            };
            if !lat_s.is_empty() {
                if let (Ok(lat), Ok(lon)) = (lat_s.parse::<f64>(), lon_s.parse::<f64>()) {
                    if valid_latlon(lat, lon) {
                        return Some(Destination::LatLon { lat, lon });
                    }
                }
            }
        }
        // ?mlat=&mlon=.
        if let (Some(lat_raw), Some(lon_raw)) =
            (url_query_param(url, "mlat"), url_query_param(url, "mlon"))
        {
            if let (Ok(lat), Ok(lon)) = (
                decode_query_value(&lat_raw).trim().parse::<f64>(),
                decode_query_value(&lon_raw).trim().parse::<f64>(),
            ) {
                if valid_latlon(lat, lon) {
                    return Some(Destination::LatLon { lat, lon });
                }
            }
        }
        return None;
    }

    // Unknown host (HERE, Bing, OSM short links, ...): opaque.
    None
}

/// Strict `lat,lon` / `lat;lon` / `lat lon` pair. The whole string must be
/// exactly two finite numbers — this is what keeps "1600 Amphitheatre..."
/// an address. Returns `None` for non-pairs AND for out-of-range pairs
/// (use [`looks_like_latlon_pair`] to tell those apart).
fn parse_latlon_pair(s: &str) -> Option<(f64, f64)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = if s.contains(',') {
        s.split(',').collect()
    } else if s.contains(';') {
        s.split(';').collect()
    } else {
        s.split_whitespace().collect()
    };
    if parts.len() != 2 {
        return None;
    }
    let lat: f64 = parts[0].trim().parse().ok()?;
    let lon: f64 = parts[1].trim().parse().ok()?;
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    if valid_latlon(lat, lon) {
        Some((lat, lon))
    } else {
        None
    }
}

/// True when the string has the SHAPE of a coordinate pair (two comma- or
/// semicolon-separated numbers, or two whitespace-separated numbers) even
/// if the values are out of range. Used to report `OutOfRange` instead of
/// misrouting "999,999" to the geocoder as an address.
fn looks_like_latlon_pair(s: &str) -> bool {
    let s = s.trim();
    let parts: Vec<&str> = if s.contains(',') {
        s.split(',').collect()
    } else if s.contains(';') {
        s.split(';').collect()
    } else {
        s.split_whitespace().collect()
    };
    if parts.len() != 2 {
        return false;
    }
    parts[0].trim().parse::<f64>().is_ok() && parts[1].trim().parse::<f64>().is_ok()
}

fn valid_latlon(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

fn first_non_empty_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|l| !l.is_empty())
}

/// First `http(s)://` token in the text (trailing punctuation trimmed).
fn first_url_token(s: &str) -> Option<String> {
    for token in s.split_whitespace() {
        let t = token.trim_matches(|c: char| {
            c == '('
                || c == ')'
                || c == '<'
                || c == '>'
                || c == '"'
                || c == '\''
                || c == '.'
                || c == ','
                || c == ';'
        });
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(t.to_string());
        }
    }
    None
}

/// Lowercase host of a URL, or `None` when there is none.
fn url_host(lower_url: &str) -> Option<String> {
    let after_scheme = lower_url.split("://").nth(1)?;
    let end = after_scheme
        .find(['/', '?', '#', ':'])
        .unwrap_or(after_scheme.len());
    Some(after_scheme[..end].to_string())
}

/// Raw (still encoded) value of a query parameter. Searches both the `?`
/// query and, for OSM-style fragments, the `#` fragment.
fn url_query_param(url: &str, key: &str) -> Option<String> {
    for section in [
        url.split('?').nth(1).unwrap_or(""),
        url.split('#').nth(1).unwrap_or(""),
    ] {
        let query = section.split('#').next().unwrap_or(section);
        for pair in query.split('&') {
            let (k, v) = match pair.find('=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => continue,
            };
            if k.eq_ignore_ascii_case(key) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Same as [`url_query_param`] but on a bare `a=1&b=2` query string (geo:
/// URIs have no `?`-prefix handling needs beyond this).
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = match pair.find('=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => continue,
        };
        if k.eq_ignore_ascii_case(key) {
            return Some(v.to_string());
        }
    }
    None
}

/// Decode a query value: `+` → space, then `%XX`. Malformed `%` sequences
/// are kept literally rather than failing the whole share.
fn decode_query_value(s: &str) -> String {
    let plus = s.replace('+', " ");
    percent_decode(&plus)
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            if let (Some(h), Some(l)) = (hex_val(bytes.get(i + 1)), hex_val(bytes.get(i + 2))) {
                out.push(((h << 4) | l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    // `%XX` may have produced UTF-8 bytes as latin-1 chars; round-trip back.
    // (Query values from maps URLs are ASCII in practice; this keeps the
    // common case correct without pulling in a decoding crate.)
    out
}

fn hex_val(b: Option<&u8>) -> Option<u8> {
    let b = *b?;
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `/@lat,lon` in a Google Maps path (also `/place/.../@lat,lon,zoom`).
fn url_path_at_coords(url: &str) -> Option<(f64, f64)> {
    for marker in ["/@", "/place/"] {
        let mut search = url;
        while let Some(i) = search.find(marker) {
            let after = &search[i + marker.len()..];
            // For /place/, skip to the next /@.
            let cand = if marker == "/place/" {
                if let Some(j) = after.find("/@") {
                    &after[j + 2..]
                } else {
                    search = after;
                    continue;
                }
            } else {
                after
            };
            let end = cand
                .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != ',' && c != '+')
                .unwrap_or(cand.len());
            let pair = &cand[..end];
            let nums: Vec<&str> = pair.split(',').collect();
            if nums.len() >= 2 {
                if let (Ok(lat), Ok(lon)) = (nums[0].parse::<f64>(), nums[1].parse::<f64>()) {
                    if valid_latlon(lat, lon) {
                        return Some((lat, lon));
                    }
                }
            }
            search = after;
        }
    }
    None
}

/// Google's embedded `!3dLAT!4dLON` markers. Takes the LAST pair (most
/// specific = the destination, earlier ones are viewport hints).
fn google_3d4d_coords(url: &str) -> Option<(f64, f64)> {
    let mut result = None;
    let mut search = url;
    while let Some(i) = search.find("!3d") {
        let lat_start = i + 3;
        let lat_end = search[lat_start..]
            .find('!')
            .map_or(search.len(), |j| lat_start + j);
        if let Some(j) = search[lat_end..].find("!4d") {
            let lon_start = lat_end + j + 3;
            let lon_end = search[lon_start..]
                .find('!')
                .map_or(search.len(), |k| lon_start + k);
            if let (Ok(lat), Ok(lon)) = (
                search[lat_start..lat_end].parse::<f64>(),
                search[lon_start..lon_end].parse::<f64>(),
            ) {
                if valid_latlon(lat, lon) {
                    result = Some((lat, lon));
                }
            }
            search = &search[lon_end..];
        } else {
            break;
        }
    }
    result
}

fn strip_prefix_case_insensitive<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latlon(lat: f64, lon: f64) -> Destination {
        Destination::LatLon { lat, lon }
    }

    fn addr(s: &str) -> Destination {
        Destination::Address(s.to_string())
    }

    // The spike-doc §5 contract vectors.
    #[test]
    fn test_contract_vectors() {
        let cases: &[(&str, Destination)] = &[
            ("geo:48.8584,2.2945", latlon(48.8584, 2.2945)),
            ("geo:48.8584,2.2945?q=Eiffel+Tower", latlon(48.8584, 2.2945)),
            (
                "geo:0,0?q=1600+Amphitheatre+Parkway",
                addr("1600 Amphitheatre Parkway"),
            ),
            ("48.8584, 2.2945", latlon(48.8584, 2.2945)),
            (
                "https://maps.google.com/?q=48.8584,2.2945",
                latlon(48.8584, 2.2945),
            ),
            (
                "https://www.google.com/maps/place/Eiffel+Tower/@48.8584,2.2945,17z",
                latlon(48.8584, 2.2945),
            ),
            (
                "https://maps.apple.com/?q=Eiffel+Tower&ll=48.8584,2.2945",
                latlon(48.8584, 2.2945),
            ),
            (
                "https://www.openstreetmap.org/#map=17/48.8584/2.2945",
                latlon(48.8584, 2.2945),
            ),
            (
                "1600 Amphitheatre Parkway, Mountain View, CA",
                addr("1600 Amphitheatre Parkway, Mountain View, CA"),
            ),
            (
                "https://maps.google.com/?q=Eiffel+Tower",
                addr("Eiffel Tower"),
            ),
        ];
        for (input, want) in cases {
            let got = parse_shared_text(input)
                .unwrap_or_else(|e| panic!("input {input:?} should parse, got error {e}"));
            assert_eq!(&got, want, "input {input:?}");
        }

        assert_eq!(parse_shared_text(""), Err(ShareParseError::Empty));
        assert_eq!(parse_shared_text("   \n  "), Err(ShareParseError::Empty));
        // Out-of-range pairs must error, never become "addresses".
        assert_eq!(
            parse_shared_text("999,999"),
            Err(ShareParseError::OutOfRange)
        );
        assert_eq!(
            parse_shared_text("geo:999,999"),
            Err(ShareParseError::OutOfRange)
        );
        assert_eq!(
            parse_shared_text(&"x".repeat(2001)),
            Err(ShareParseError::TooLong)
        );
    }

    #[test]
    fn test_android_share_multiline_prefers_url() {
        // Timdorr-doc shape: "address\n\nhttps://...".
        let got =
            parse_shared_text("Eiffel Tower\n\nhttps://maps.google.com/?q=48.8584,2.2945").unwrap();
        assert_eq!(got, latlon(48.8584, 2.2945));
    }

    #[test]
    fn test_separators_and_whitespace() {
        assert_eq!(
            parse_shared_text("48.8584;2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
        assert_eq!(
            parse_shared_text("48.8584 2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
        assert_eq!(
            parse_shared_text("  48.8584 , 2.2945  ").unwrap(),
            latlon(48.8584, 2.2945)
        );
        assert_eq!(
            parse_shared_text("-33.8688, 151.2093").unwrap(),
            latlon(-33.8688, 151.2093)
        );
    }

    #[test]
    fn test_house_number_is_not_coordinates() {
        // Regression: a street address starting with a number must not
        // parse as a coordinate pair.
        assert_eq!(
            parse_shared_text("1600 Amphitheatre Parkway").unwrap(),
            addr("1600 Amphitheatre Parkway")
        );
    }

    #[test]
    fn test_no_silent_swap() {
        // 200°E is invalid; auto-swapping to (2.29, 48.85) would route to
        // the wrong continent. Reject instead.
        assert_eq!(
            parse_shared_text("48.8584, 200"),
            Err(ShareParseError::OutOfRange)
        );
    }

    #[test]
    fn test_osm_query_params() {
        assert_eq!(
            parse_shared_text("https://www.openstreetmap.org/?mlat=48.8584&mlon=2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
    }

    #[test]
    fn test_google_3d4d_embedded() {
        let url = "https://www.google.com/maps/place/X/@48.0,2.0,17z/data=!3m1!4b1!4m6!3m5!1s0x0!7e2!8m2!3d48.8584!4d2.2945";
        assert_eq!(parse_shared_text(url).unwrap(), latlon(48.8584, 2.2945));
    }

    #[test]
    fn test_opaque_url_passes_through() {
        let url = "https://share.here.com/l/abc123";
        assert_eq!(parse_shared_text(url).unwrap(), addr(url));
    }

    #[test]
    fn test_geo_case_insensitive() {
        assert_eq!(
            parse_shared_text("GEO:48.8584,2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
    }
}
