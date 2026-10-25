//! RFC 7692 `permessage-deflate` negotiation.
//!
//! The client makes Chromium's documented offer
//! (`permessage-deflate; client_max_window_bits`); the server's response
//! decides the parameters. A response that we did not offer, duplicates a
//! parameter, omits a required value or uses an unknown parameter is a typed
//! failure and the caller refuses the connection instead of guessing.

use std::fmt;

/// Chromium's documented client offer, and the one the official Electron
/// client makes (audit A9). Only parameters the implementation honours are
/// offered; `server_max_window_bits` is not, and a server response carrying it
/// is rejected as unsolicited.
pub const OFFER: &str = "permessage-deflate; client_max_window_bits";

/// Which response parameters the client offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// We allow the server to lower our compression window.
    pub client_max_window_bits: bool,
    /// We allow the server to declare its compression window.
    pub server_max_window_bits: bool,
}

impl Offer {
    /// The production offer.
    pub const CHROMIUM: Self = Self {
        client_max_window_bits: true,
        server_max_window_bits: false,
    };

    /// Parses one `Sec-WebSocket-Extensions` offer value for this extension.
    ///
    /// Used by the tests and by `Offered::production` to keep [`OFFER`] and the
    /// semantics in one place.
    pub fn parse(value: &str) -> Result<Self, ExtensionError> {
        let extensions = parse_extensions(value)?;
        if extensions.len() != 1 {
            return Err(ExtensionError::MultipleExtensions);
        }
        let extension = &extensions[0];
        if !extension.name.eq_ignore_ascii_case("permessage-deflate") {
            return Err(ExtensionError::UnknownExtension);
        }
        let mut offer = Self {
            client_max_window_bits: false,
            server_max_window_bits: false,
        };
        for parameter in &extension.parameters {
            match parameter.name.as_str() {
                "client_max_window_bits" => {
                    if parameter.value.is_some() {
                        return Err(ExtensionError::UnexpectedValue("client_max_window_bits"));
                    }
                    offer.client_max_window_bits = true;
                }
                "server_max_window_bits" => {
                    if parameter.value.is_some() {
                        return Err(ExtensionError::UnexpectedValue("server_max_window_bits"));
                    }
                    offer.server_max_window_bits = true;
                }
                "client_no_context_takeover" => {
                    if parameter.value.is_some() {
                        return Err(ExtensionError::UnexpectedValue(
                            "client_no_context_takeover",
                        ));
                    }
                }
                "server_no_context_takeover" => {
                    if parameter.value.is_some() {
                        return Err(ExtensionError::UnexpectedValue(
                            "server_no_context_takeover",
                        ));
                    }
                }
                _ => return Err(ExtensionError::UnknownParameter),
            }
        }
        Ok(offer)
    }
}

impl Default for Offer {
    fn default() -> Self {
        Self::CHROMIUM
    }
}

/// The negotiated parameters. Absent `*_max_window_bits` default to 15.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerMessageDeflate {
    /// The window the client may use when compressing. 15 unless the server
    /// lowered it.
    pub client_max_window_bits: u8,
    /// The window the server declared it uses. 15 unless the server declared
    /// less; decoding with a larger window is always safe.
    pub server_max_window_bits: u8,
    /// The client must not reuse its compression context between messages.
    pub client_no_context_takeover: bool,
    /// The server does not reuse its compression context between messages.
    pub server_no_context_takeover: bool,
}

impl PerMessageDeflate {
    /// When the negotiated client window is smaller than the compressor's
    /// 15-bit window, outgoing messages are sent uncompressed (RSV1=0), which
    /// RFC 7692 permits and which honours the limit exactly.
    pub const fn client_may_compress(&self) -> bool {
        self.client_max_window_bits == 15
    }
}

impl Default for PerMessageDeflate {
    fn default() -> Self {
        Self {
            client_max_window_bits: 15,
            server_max_window_bits: 15,
            client_no_context_takeover: false,
            server_no_context_takeover: false,
        }
    }
}

/// Categorical: no header text is retained or printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionError {
    /// The header value is not the RFC 6455 extension grammar.
    BadSyntax,
    /// The response names an extension the client did not offer.
    UnknownExtension,
    /// The response accepted more than one extension.
    MultipleExtensions,
    /// A parameter not defined by RFC 7692.
    UnknownParameter,
    /// The same parameter appeared twice.
    DuplicateParameter,
    /// A window-bits parameter arrived without a value.
    MissingValue(&'static str),
    /// A value arrived on a parameter that must be bare (or vice versa).
    UnexpectedValue(&'static str),
    /// A window-bits value is not an integer in 8..=15.
    InvalidWindowBits(&'static str),
    /// A parameter the client never offered.
    UnofferedParameter(&'static str),
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadSyntax => "malformed Sec-WebSocket-Extensions",
            Self::UnknownExtension => "unoffered WebSocket extension",
            Self::MultipleExtensions => "multiple WebSocket extensions accepted",
            Self::UnknownParameter => "unknown permessage-deflate parameter",
            Self::DuplicateParameter => "duplicate permessage-deflate parameter",
            Self::MissingValue(_) => "permessage-deflate parameter needs a value",
            Self::UnexpectedValue(_) => "permessage-deflate parameter takes no value",
            Self::InvalidWindowBits(_) => "permessage-deflate window bits out of range",
            Self::UnofferedParameter(_) => "unoffered permessage-deflate parameter",
        })
    }
}

impl std::error::Error for ExtensionError {}

/// The server declined the extension: the connection works exactly like one
/// without it.
pub fn negotiate(
    offer: Offer,
    response_values: &[&str],
) -> Result<Option<PerMessageDeflate>, ExtensionError> {
    if response_values.is_empty() {
        return Ok(None);
    }
    let mut extensions = Vec::new();
    for value in response_values {
        extensions.extend(parse_extensions(value)?);
    }
    if extensions.is_empty() {
        return Err(ExtensionError::BadSyntax);
    }
    if extensions.len() > 1 {
        return Err(ExtensionError::MultipleExtensions);
    }
    let extension = &extensions[0];
    if !extension.name.eq_ignore_ascii_case("permessage-deflate") {
        return Err(ExtensionError::UnknownExtension);
    }
    let mut negotiated = PerMessageDeflate::default();
    let mut seen = [false; 4];
    for parameter in &extension.parameters {
        let (index, name) = match parameter.name.as_str() {
            "server_no_context_takeover" => (0, "server_no_context_takeover"),
            "client_no_context_takeover" => (1, "client_no_context_takeover"),
            "server_max_window_bits" => (2, "server_max_window_bits"),
            "client_max_window_bits" => (3, "client_max_window_bits"),
            _ => return Err(ExtensionError::UnknownParameter),
        };
        if seen[index] {
            return Err(ExtensionError::DuplicateParameter);
        }
        seen[index] = true;
        match index {
            0 | 1 => {
                if parameter.value.is_some() {
                    return Err(ExtensionError::UnexpectedValue(name));
                }
                if index == 0 {
                    negotiated.server_no_context_takeover = true;
                } else {
                    negotiated.client_no_context_takeover = true;
                }
            }
            2 => {
                if !offer.server_max_window_bits {
                    return Err(ExtensionError::UnofferedParameter(name));
                }
                let bits = window_bits(parameter.value.as_deref(), name)?;
                negotiated.server_max_window_bits = bits;
            }
            3 => {
                if !offer.client_max_window_bits {
                    return Err(ExtensionError::UnofferedParameter(name));
                }
                let bits = window_bits(parameter.value.as_deref(), name)?;
                negotiated.client_max_window_bits = bits;
            }
            _ => unreachable!(),
        }
    }
    Ok(Some(negotiated))
}

fn window_bits(value: Option<&str>, name: &'static str) -> Result<u8, ExtensionError> {
    let value = value.ok_or(ExtensionError::MissingValue(name))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ExtensionError::InvalidWindowBits(name));
    }
    let Ok(bits) = value.parse::<u8>() else {
        return Err(ExtensionError::InvalidWindowBits(name));
    };
    if !(8..=15).contains(&bits) {
        return Err(ExtensionError::InvalidWindowBits(name));
    }
    Ok(bits)
}

#[derive(Debug)]
struct Extension {
    name: String,
    parameters: Vec<Parameter>,
}

#[derive(Debug)]
struct Parameter {
    name: String,
    value: Option<String>,
}

fn parse_extensions(value: &str) -> Result<Vec<Extension>, ExtensionError> {
    if value.trim().is_empty() {
        return Err(ExtensionError::BadSyntax);
    }
    let mut extensions = Vec::new();
    for entry in split_top_level(value, ',')? {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(ExtensionError::BadSyntax);
        }
        let parts = split_top_level(entry, ';')?;
        let name = parts[0].trim();
        if !is_token(name) {
            return Err(ExtensionError::BadSyntax);
        }
        let mut parameters = Vec::new();
        for part in &parts[1..] {
            let part = part.trim();
            if part.is_empty() {
                return Err(ExtensionError::BadSyntax);
            }
            let (name, value) = match part.split_once('=') {
                Some((name, value)) => (name.trim(), Some(unquote(value.trim())?)),
                None => (part, None),
            };
            if !is_token(name) {
                return Err(ExtensionError::BadSyntax);
            }
            parameters.push(Parameter {
                name: name.to_ascii_lowercase(),
                value,
            });
        }
        extensions.push(Extension {
            name: name.to_owned(),
            parameters,
        });
    }
    Ok(extensions)
}

/// Splits on `separator` outside a quoted string. RFC 6455 parameter values may
/// be quoted, so a naive `split` is not enough.
fn split_top_level(value: &str, separator: char) -> Result<Vec<&str>, ExtensionError> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            character if character == separator && !quoted => {
                parts.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return Err(ExtensionError::BadSyntax);
    }
    parts.push(&value[start..]);
    Ok(parts)
}

fn unquote(value: &str) -> Result<String, ExtensionError> {
    if value.starts_with('"') {
        if value.len() < 2 || !value.ends_with('"') {
            return Err(ExtensionError::BadSyntax);
        }
        let inner = &value[1..value.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for character in inner.chars() {
            if escaped {
                out.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Err(ExtensionError::BadSyntax);
            } else {
                out.push(character);
            }
        }
        if escaped {
            return Err(ExtensionError::BadSyntax);
        }
        Ok(out)
    } else {
        if value.is_empty() || !is_token(value) {
            return Err(ExtensionError::BadSyntax);
        }
        Ok(value.to_owned())
    }
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offer_is_chromiums_documented_offer() {
        assert_eq!(OFFER, "permessage-deflate; client_max_window_bits");
        assert_eq!(Offer::parse(OFFER), Ok(Offer::CHROMIUM));
    }

    #[test]
    fn absent_response_means_the_extension_is_declined() {
        assert_eq!(negotiate(Offer::CHROMIUM, &[]), Ok(None));
    }

    #[test]
    fn a_bare_response_accepts_with_the_default_windows() {
        assert_eq!(
            negotiate(Offer::CHROMIUM, &["permessage-deflate"]),
            Ok(Some(PerMessageDeflate::default()))
        );
    }

    #[test]
    fn each_supported_parameter_is_honoured() {
        assert_eq!(
            negotiate(
                Offer::CHROMIUM,
                &["permessage-deflate; server_no_context_takeover"]
            ),
            Ok(Some(PerMessageDeflate {
                server_no_context_takeover: true,
                ..PerMessageDeflate::default()
            }))
        );
        assert_eq!(
            negotiate(
                Offer::CHROMIUM,
                &["permessage-deflate; client_no_context_takeover"]
            ),
            Ok(Some(PerMessageDeflate {
                client_no_context_takeover: true,
                ..PerMessageDeflate::default()
            }))
        );
        assert_eq!(
            negotiate(
                Offer::CHROMIUM,
                &["permessage-deflate; client_max_window_bits=12"],
            ),
            Ok(Some(PerMessageDeflate {
                client_max_window_bits: 12,
                ..PerMessageDeflate::default()
            }))
        );
        assert_eq!(
            negotiate(
                Offer {
                    server_max_window_bits: true,
                    ..Offer::CHROMIUM
                },
                &["permessage-deflate; server_max_window_bits=10"],
            ),
            Ok(Some(PerMessageDeflate {
                server_max_window_bits: 10,
                ..PerMessageDeflate::default()
            }))
        );
    }

    #[test]
    fn parameters_are_case_insensitive_and_quoted_values_parse() {
        assert_eq!(
            negotiate(
                Offer::CHROMIUM,
                &["PerMessage-Deflate; CLIENT_MAX_WINDOW_BITS=\"11\""],
            ),
            Ok(Some(PerMessageDeflate {
                client_max_window_bits: 11,
                ..PerMessageDeflate::default()
            }))
        );
    }

    #[test]
    fn every_response_parameter_combination_is_read_from_the_header() {
        assert_eq!(
            negotiate(
                Offer {
                    server_max_window_bits: true,
                    ..Offer::CHROMIUM
                },
                &["permessage-deflate; server_no_context_takeover; \
                   client_no_context_takeover; server_max_window_bits=9; \
                   client_max_window_bits=9"],
            ),
            Ok(Some(PerMessageDeflate {
                client_max_window_bits: 9,
                server_max_window_bits: 9,
                client_no_context_takeover: true,
                server_no_context_takeover: true,
            }))
        );
    }

    #[test]
    fn unsolicited_extensions_and_parameters_are_typed_failures() {
        assert_eq!(
            negotiate(Offer::CHROMIUM, &["x-webkit-deflate-frame"]),
            Err(ExtensionError::UnknownExtension)
        );
        assert_eq!(
            negotiate(
                Offer::CHROMIUM,
                &["permessage-deflate; server_max_window_bits=15"],
            ),
            Err(ExtensionError::UnofferedParameter("server_max_window_bits"))
        );
        assert_eq!(
            negotiate(Offer::CHROMIUM, &["permessage-deflate, permessage-deflate"],),
            Err(ExtensionError::MultipleExtensions)
        );
    }

    #[test]
    fn malformed_duplicate_unknown_and_missing_values_are_rejected() {
        for (header, error) in [
            (
                "permessage-deflate; unknown_parameter",
                ExtensionError::UnknownParameter,
            ),
            (
                "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
                ExtensionError::DuplicateParameter,
            ),
            (
                "permessage-deflate; server_no_context_takeover=1",
                ExtensionError::UnexpectedValue("server_no_context_takeover"),
            ),
            (
                "permessage-deflate; client_max_window_bits",
                ExtensionError::MissingValue("client_max_window_bits"),
            ),
            (
                "permessage-deflate; client_max_window_bits=7",
                ExtensionError::InvalidWindowBits("client_max_window_bits"),
            ),
            (
                "permessage-deflate; client_max_window_bits=16",
                ExtensionError::InvalidWindowBits("client_max_window_bits"),
            ),
            (
                "permessage-deflate; client_max_window_bits=abc",
                ExtensionError::InvalidWindowBits("client_max_window_bits"),
            ),
            ("", ExtensionError::BadSyntax),
            ("permessage-deflate;", ExtensionError::BadSyntax),
            (
                "permessage-deflate; \"unterminated",
                ExtensionError::BadSyntax,
            ),
        ] {
            assert_eq!(
                negotiate(Offer::CHROMIUM, &[header]),
                Err(error),
                "{header}"
            );
        }
    }

    #[test]
    fn a_lowered_client_window_disables_outgoing_compression() {
        assert!(PerMessageDeflate::default().client_may_compress());
        assert!(
            !PerMessageDeflate {
                client_max_window_bits: 14,
                ..PerMessageDeflate::default()
            }
            .client_may_compress()
        );
    }
}
