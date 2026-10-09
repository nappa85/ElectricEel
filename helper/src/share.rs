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
use url::{form_urlencoded, Url};

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
        if let Some(dest) = parse_map_url(&url)? {
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
        return Err(ShareParseError::Empty);
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

/// `Ok(Some)` = parsed, `Err` = empty destination or out-of-range coordinates.
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
        .filter(|s| !s.trim().is_empty());

    if let Some((lat, lon)) = parse_latlon_pair(coords_part.trim()) {
        if lat == 0.0 && lon == 0.0 {
            // `geo:0,0?q=...` is an address search, not a point in the
            // Gulf of Guinea.
            if let Some(addr) = query_addr {
                return Ok(Some(Destination::Address(addr)));
            }
            return Err(ShareParseError::Empty);
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

/// Try a URL from a share payload. Host-agnostic on purpose: instead of a
/// per-service allowlist, structural rules extract coordinates from any
/// map URL, and anything unparseable stays shareable address text
/// (returned as `None` here; the caller sends the URL itself).
///
/// A bare "first two floats" scan would be simpler but silently wrong:
/// OSM `#map=17/lat/lon` leads with the zoom, `ll` precedes the portal
/// `pll` in intel links, viewports precede `!3d/!4d` destinations, and
/// addresses like "Via Roma 45, ..." contain small numbers. So:
/// - explicit destinations (`pll`, `destination`, `daddr`) beat all centers;
/// - only recognized coordinate parameters are used, never route origins or
///   arbitrary numeric query values;
/// - `lat`+`lon` split across two params (Bing `cp` uses `~`, `OSMAnd`
///   `?lat=&lon=`);
/// - path patterns `/@lat,lon`, `!3dLAT!4dLON`, and exactly-two
///   bare-numeric slash runs (OSM `#map=z/lat/lon`, zoom excluded
///   because `map=17` is not bare-numeric);
/// - free text is NEVER float-scanned (house numbers would hijack it).
///
/// Never errors on opaque URLs — an unparseable URL is still shareable text
/// (returned as `None` here; the caller sends the URL itself). Coordinate-
/// looking values that are out of range DO error (`OutOfRange`), matching
/// the bare-pair contract: a `daddr=999,999` must fail fast like bare
/// "999,999", never become `Address("999,999")` for the geocoder.
fn parse_map_url(url: &str) -> Result<Option<Destination>, ShareParseError> {
    let parsed = Url::parse(url).ok();
    let Some(parsed) = parsed else {
        return Ok(None);
    };
    let params: Vec<_> = parsed
        .query_pairs()
        .chain(form_urlencoded::parse(
            parsed.fragment().unwrap_or("").as_bytes(),
        ))
        .collect();
    let param = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_ref())
    };
    // Coordinate-looking but out-of-range values error instead of becoming
    // addresses, mirroring the bare-pair `looks_like_latlon_pair` check.
    let check_range = |value: &str| -> Result<(), ShareParseError> {
        if parse_latlon_pair(value).is_none() && looks_like_latlon_pair(value) {
            return Err(ShareParseError::OutOfRange);
        }
        Ok(())
    };

    // A destination may be coordinates OR text; neither may be overridden by
    // an origin or viewport just because those happen to be numeric.
    for key in ["pll", "destination", "daddr"] {
        if let Some(value) = param(key).filter(|v| !v.trim().is_empty()) {
            check_range(value)?;
            return Ok(Some(destination_value(value)));
        }
    }
    // Search coordinates take precedence over the map center. Text searches
    // remain a fallback when a URL includes an exact place coordinate.
    for key in ["q", "query"] {
        if let Some(value) = param(key).filter(|v| !v.trim().is_empty()) {
            if parse_latlon_pair(value).is_some() {
                let (lat, lon) = parse_latlon_pair(value).unwrap_or((0.0, 0.0));
                return Ok(Some(Destination::LatLon { lat, lon }));
            }
            check_range(value)?;
        }
    }
    if let Some((lat, lon)) = google_3d4d_coords(parsed.path())? {
        return Ok(Some(Destination::LatLon { lat, lon }));
    }
    // lat + lon split across two params.
    let lat_val = ["lat", "latitude", "mlat"]
        .iter()
        .find_map(|k| param(k))
        .map(|v| v.trim().to_string());
    let lon_val = ["lon", "lng", "long", "longitude", "mlon"]
        .iter()
        .find_map(|k| param(k))
        .map(|v| v.trim().to_string());
    if let (Some(lat_s), Some(lon_s)) = (lat_val, lon_val) {
        if let (Ok(lat), Ok(lon)) = (lat_s.parse::<f64>(), lon_s.parse::<f64>()) {
            if valid_latlon(lat, lon) {
                return Ok(Some(Destination::LatLon { lat, lon }));
            }
            if lat.is_finite() && lon.is_finite() {
                return Err(ShareParseError::OutOfRange);
            }
        }
    }
    for key in ["ll", "cp"] {
        if let Some(value) = param(key).filter(|v| !v.trim().is_empty()) {
            if let Some((lat, lon)) = parse_latlon_pair(value) {
                return Ok(Some(Destination::LatLon { lat, lon }));
            }
            check_range(value)?;
        }
    }
    // Path patterns, most specific first.
    if let Some((lat, lon)) = url_path_at_coords(parsed.path())? {
        return Ok(Some(Destination::LatLon { lat, lon }));
    }
    // Tile resources contain z/x/y indices, not geographic coordinates.
    let is_tile = [".png", ".jpg", ".jpeg", ".webp"]
        .iter()
        .any(|extension| parsed.path().to_ascii_lowercase().ends_with(extension));
    if let Some((lat, lon)) = if is_tile {
        None
    } else {
        slash_run_coords(url)?
    } {
        return Ok(Some(Destination::LatLon { lat, lon }));
    }
    // A named text param (address or opaque link target) becomes the
    // address; otherwise the caller sends the whole URL.
    for key in ["q", "query"] {
        if let Some(val) = param(key) {
            let text = val.trim().to_string();
            if !text.is_empty() {
                return Ok(Some(Destination::Address(text)));
            }
        }
    }
    Ok(None)
}

fn destination_value(value: &str) -> Destination {
    parse_latlon_pair(value).map_or_else(
        || Destination::Address(value.trim().to_string()),
        |(lat, lon)| Destination::LatLon { lat, lon },
    )
}

/// Coordinates from slash-separated path/fragment segments: the last
/// exactly-two run of bare-numeric segments (e.g. OSM `#map=17/lat/lon`
/// — `map=17` is not bare-numeric so the zoom never leaks in; tile
/// `z/x/y` runs fail the range check downstream in `parse_latlon_pair`
/// semantics via [`valid_latlon`]).
fn slash_run_coords(url: &str) -> Result<Option<(f64, f64)>, ShareParseError> {
    let mut run: Vec<f64> = Vec::new();
    let mut best: Option<(f64, f64)> = None;
    let flush = |run: &mut Vec<f64>, best: &mut Option<(f64, f64)>| {
        if run.len() == 2 {
            let (lat, lon) = (run[0], run[1]);
            *best = Some((lat, lon));
        }
        run.clear();
    };
    let path_and_frag = format!(
        "{}#{}",
        url.split(['?', '#']).next().unwrap_or(url),
        url.split('#').nth(1).unwrap_or("")
    );
    for seg in path_and_frag.split('/') {
        // Strip query/form wrappers: only truly bare segments count.
        if seg.contains(['?', '&', '=', '@', '!']) {
            flush(&mut run, &mut best);
            continue;
        }
        match seg.trim().parse::<f64>() {
            Ok(v) if v.is_finite() => run.push(v),
            _ => flush(&mut run, &mut best),
        }
    }
    flush(&mut run, &mut best);
    checked_path_coords(best)
}

/// Strict pair: the whole string must be exactly two finite numbers
/// separated by `,` `;` `~` `|` or whitespace (Bing `cp=lat~lon`,
/// parenthesized copies) — this is what keeps "1600 Amphitheatre..."
/// an address. Returns `None` for non-pairs AND for out-of-range pairs
/// (use [`looks_like_latlon_pair`] to tell those apart).
fn parse_latlon_pair(s: &str) -> Option<(f64, f64)> {
    let (lat, lon) = numeric_pair(s)?;
    valid_latlon(lat, lon).then_some((lat, lon))
}

fn numeric_pair(s: &str) -> Option<(f64, f64)> {
    let s = s
        .trim()
        .trim_matches(|c: char| c == '(' || c == ')' || c == '[' || c == ']');
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = if s.contains(',') {
        s.split(',').collect()
    } else if s.contains(';') {
        s.split(';').collect()
    } else if s.contains('~') {
        s.split('~').collect()
    } else if s.contains('|') {
        s.split('|').collect()
    } else {
        s.split_whitespace().collect()
    };
    if parts.len() != 2 {
        return None;
    }
    let lat: f64 = parts[0].trim().parse().ok()?;
    let lon: f64 = parts[1].trim().parse().ok()?;
    Some((lat, lon))
}

/// True when the string has the SHAPE of a coordinate pair (two comma- or
/// semicolon-separated numbers, or two whitespace-separated numbers) even
/// if the values are out of range. Used to report `OutOfRange` instead of
/// misrouting "999,999" to the geocoder as an address.
fn looks_like_latlon_pair(s: &str) -> bool {
    numeric_pair(s).is_some()
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
                || c == '!'
                || c == '?'
                || c == ':'
        });
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(t.to_string());
        }
    }
    None
}

/// Decoded value of a query parameter in a bare `a=1&b=2`
/// query string (geo: URIs).
fn query_param(query: &str, key: &str) -> Option<String> {
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if k.eq_ignore_ascii_case(key) {
            return Some(v.into_owned());
        }
    }
    None
}

/// `/@lat,lon` in a Google Maps path (also `/place/.../@lat,lon,zoom`).
fn url_path_at_coords(url: &str) -> Result<Option<(f64, f64)>, ShareParseError> {
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
                    return checked_path_coords(Some((lat, lon)));
                }
            }
            search = after;
        }
    }
    Ok(None)
}

/// Google's embedded `!3dLAT!4dLON` markers. Takes the LAST pair (most
/// specific = the destination, earlier ones are viewport hints).
fn google_3d4d_coords(url: &str) -> Result<Option<(f64, f64)>, ShareParseError> {
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
                result = Some((lat, lon));
            }
            search = &search[lon_end..];
        } else {
            break;
        }
    }
    checked_path_coords(result)
}

fn checked_path_coords(coords: Option<(f64, f64)>) -> Result<Option<(f64, f64)>, ShareParseError> {
    if coords.is_some_and(|(lat, lon)| !valid_latlon(lat, lon)) {
        return Err(ShareParseError::OutOfRange);
    }
    Ok(coords)
}

fn strip_prefix_case_insensitive<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    {
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

    #[test]
    fn review_unicode_addresses_do_not_panic() {
        for text in ["東京都千代田区", "🏠 Home", "a東京", "서울특별시"] {
            assert_eq!(parse_shared_text(text), Ok(addr(text)), "input {text:?}");
        }
    }

    #[test]
    fn review_directions_use_destination_not_origin_or_viewport() {
        let cases = [
            (
                "https://www.google.com/maps/dir/?api=1&origin=48.0,2.0&destination=48.8584,2.2945",
                latlon(48.8584, 2.2945),
            ),
            (
                "https://maps.apple.com/?saddr=48.0,2.0&daddr=48.8584,2.2945",
                latlon(48.8584, 2.2945),
            ),
            (
                "https://maps.google.com/?ll=48.0,2.0&daddr=Eiffel+Tower",
                addr("Eiffel Tower"),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_shared_text(input), Ok(expected), "input {input:?}");
        }
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
    fn test_generic_param_scan() {
        // Bing cp with tilde separator.
        assert_eq!(
            parse_shared_text("https://www.bing.com/maps?cp=48.8584~2.2945&lvl=16").unwrap(),
            latlon(48.8584, 2.2945)
        );
        // OSMAnd-style split lat/lon params.
        assert_eq!(
            parse_shared_text("http://osmand.net/go?lat=48.8584&lon=2.2945&z=15").unwrap(),
            latlon(48.8584, 2.2945)
        );
        // Zoom-first query order: singles can never match as pairs.
        assert_eq!(
            parse_shared_text("https://www.waze.com/ul?z=10&ll=48.8584,2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
        // Tile z/x/y: last pair out of range, stays opaque, never coords.
        let tile = "https://tile.openstreetmap.org/17/65535/48351.png";
        assert_eq!(parse_shared_text(tile).unwrap(), addr(tile));
    }

    #[test]
    fn test_addresses_with_numbers_stay_addresses() {
        // The generic scan only applies inside URLs: bare text with small
        // numbers is an address, never silently hijacked as coordinates
        // (contrast OSM #map, where structure disambiguates).
        assert_eq!(
            parse_shared_text("Via Roma 45, 09125 Cagliari").unwrap(),
            addr("Via Roma 45, 09125 Cagliari")
        );
        assert_eq!(
            parse_shared_text("1600 Amphitheatre Parkway").unwrap(),
            addr("1600 Amphitheatre Parkway")
        );
    }

    #[test]
    fn test_geo_case_insensitive() {
        assert_eq!(
            parse_shared_text("GEO:48.8584,2.2945").unwrap(),
            latlon(48.8584, 2.2945)
        );
    }

    #[test]
    fn test_ingress_intel_links() {
        // Stock / IITC Mobile share: pll is the portal (destination).
        assert_eq!(
            parse_shared_text(
                "https://intel.ingress.com/intel?ll=48.85,2.29&z=17&pll=48.8584,2.2945"
            )
            .unwrap(),
            latlon(48.8584, 2.2945)
        );
        // IITC desktop permalink without /intel path.
        assert_eq!(
            parse_shared_text("https://intel.ingress.com/?pll=48.8584,2.2945&z=19").unwrap(),
            latlon(48.8584, 2.2945)
        );
        // No pll: fall back to the map center ll.
        assert_eq!(
            parse_shared_text("https://intel.ingress.com/intel?ll=48.8584,2.2945&z=17").unwrap(),
            latlon(48.8584, 2.2945)
        );
        // Portal name + link, the Android share shape.
        assert_eq!(
            parse_shared_text(
                "Tour Eiffel\n\nhttps://intel.ingress.com/intel?ll=48.85,2.29&z=17&pll=48.8584,2.2945"
            )
            .unwrap(),
            latlon(48.8584, 2.2945)
        );
    }

    #[test]
    fn test_percent_decoding_preserves_utf8() {
        // %C3%A9 is UTF-8 for é. Decoding each %XX byte as a latin-1 char
        // yields "CafÃ©" (two codepoints) instead of "Café".
        assert_eq!(query_param("q=Caf%C3%A9", "q").as_deref(), Some("Café"));
        assert_eq!(
            parse_shared_text("https://maps.google.com/?q=Caf%C3%A9").unwrap(),
            addr("Café")
        );
    }

    #[test]
    fn test_parenthesized_out_of_range_is_not_address() {
        // parse_latlon_pair trims ()/[] but looks_like_latlon_pair does not,
        // so a bracketed out-of-range pair falls through to Address instead
        // of OutOfRange and would be sent to the geocoder.
        assert_eq!(
            parse_shared_text("(999,999)"),
            Err(ShareParseError::OutOfRange)
        );
        assert_eq!(
            parse_shared_text("[999,999]"),
            Err(ShareParseError::OutOfRange)
        );
        assert_eq!(
            parse_shared_text("(48.8584, 200)"),
            Err(ShareParseError::OutOfRange)
        );
    }

    #[test]
    fn review_url_embedded_out_of_range_is_not_silent_address() {
        // Bare "999,999" is Err(OutOfRange) (see test_contract_vectors), but
        // a URL-embedded destination goes through destination_value(), which
        // maps ANY parse failure — including out-of-range — to
        // Address(value). "daddr=999,999" therefore becomes Ok(Address("999,999"))
        // and would be sent to the car geocoder as an address instead of
        // failing fast like the identical bare pair.
        for url in [
            "https://maps.google.com/?daddr=999,999",
            "https://www.google.com/maps/dir/?api=1&destination=999,999",
            "https://intel.ingress.com/?pll=999,999&z=19",
        ] {
            let got = parse_shared_text(url);
            assert_ne!(
                got,
                Ok(addr("999,999")),
                "URL-embedded out-of-range coords must not become a bare address: {url:?} got {got:?}"
            );
        }
    }

    #[test]
    fn production_path_coords_out_of_range_must_error_not_address() {
        // Bare "999,999" is Err(OutOfRange), and param-embedded "daddr=999,999"
        // is guarded by check_range — but path-embedded coordinates
        // ("/@lat,lon", "!3d/!4d", OSM "#map=z/lat/lon") silently fall through
        // to Ok(None) and become Ok(Address(url)) for a lone URL, routing an
        // obvious coordinate typo to the car geocoder instead of failing fast
        // like the identical bare pair.
        for url in [
            "https://www.google.com/maps/@999,999,17z",
            "https://www.google.com/maps/place/X/@48.0,2.0,17z/data=!3m1!4b1!4m6!3m5!1s0x0!7e2!8m2!3d999!4d999",
            "https://www.openstreetmap.org/#map=17/999/999",
            "https://www.openstreetmap.org/#map=17/200/2.2945",
        ] {
            let got = parse_shared_text(url);
            assert_eq!(
                got,
                Err(ShareParseError::OutOfRange),
                "path-embedded out-of-range coords must error like bare pairs: {url:?} got {got:?}"
            );
        }
    }

    #[test]
    fn production_geo_zero_without_query_must_error_not_literal() {
        // geo:0,0 without ?q= is the RFC 5870 "no destination" placeholder.
        // parse_geo_uri returns Ok(None) for it, and the caller falls through
        // to Ok(Address("geo:0,0")) — sending the literal string "geo:0,0" to
        // the car geocoder. It must fail instead of becoming an address.
        for input in ["geo:0,0", "geo:0,0;u=10", "GEO:0,0"] {
            let got = parse_shared_text(input);
            assert!(
                got.is_err(),
                "placeholder {input:?} must error, not become {got:?}"
            );
        }
    }
}
