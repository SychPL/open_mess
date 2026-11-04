//! The host's offer in Teams' SDP dialect (MS-SDP) and Teams' answer back to standard SDP, for a
//! live-captions session's data channel (spec 2026-10-29-live-captions, 5.3; docs/protocols/teams.md
//! 10.7 and "Live results" 16 and 18). Strict: one data section only, known lines only, bounded
//! sizes; anything else is an error, never a guess.

use crate::{ProtoError, incompatible};

/// Bytes of an SDP the module converts, either way.
const MAX_SDP: usize = 16 * 1024;
const MAX_LINE: usize = 512;
const MAX_LINES: usize = 128;
/// The SCTP port of the data channel; Teams' answer names none (docs, "Live results" 16).
const SCTP_PORT: &str = "a=sctp-port:5000";

/// The lines of an SDP within the limits, `\r` dropped.
fn lines(sdp: &str) -> Result<Vec<&str>, ProtoError> {
    if sdp.len() > MAX_SDP {
        return Err(incompatible("sdp: too large"));
    }
    let lines: Vec<&str> = sdp.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()).collect();
    if lines.len() > MAX_LINES || lines.iter().any(|l| l.len() > MAX_LINE) {
        return Err(incompatible("sdp: too many or too long lines"));
    }
    Ok(lines)
}

/// The attribute name of an `a=` line, for errors: never its value, which may hold addresses.
fn unknown(line: &str) -> ProtoError {
    let name = line.split([':', ' ']).next().unwrap_or("").chars().take(32).collect::<String>();
    incompatible(&format!("sdp: unexpected line {name}"))
}

/// Splits at the `m=` line: the session part and the one media section.
fn one_section<'a>(lines: &[&'a str]) -> Result<(Vec<&'a str>, &'a str, Vec<&'a str>), ProtoError> {
    let mut sections = lines.iter().enumerate().filter(|(_, l)| l.starts_with("m="));
    let (at, media) = sections.next().ok_or_else(|| incompatible("sdp: no media section"))?;
    if sections.next().is_some() {
        return Err(incompatible("sdp: more than the data section"));
    }
    Ok((lines.iter().take(at).copied().collect(), media, lines.iter().skip(at + 1).copied().collect()))
}

/// The web client's MS-SDP of the host's offer (`toMsSdp` with `Kz.modify` in
/// `calling-pluginless`), as the probe sent it in the data-only join that gave captions (docs,
/// "Live results" 18): the section becomes `m=x-data <port> RTP/SAVP 127 126` with the x-data
/// lines, a label and the session's fingerprint; the default host candidate (the first IPv4 UDP
/// host candidate of component 1) is the only one, and its address and port are on the `m=` and
/// `c=` lines. `ssrc` is the section's `a=x-ssrc-range`.
pub fn to_ms(offer: &str, ssrc: u32) -> Result<String, ProtoError> {
    let lines = lines(offer)?;
    let (session, media, section) = one_section(&lines)?;
    let fields: Vec<&str> = media.split(' ').collect();
    if fields.len() != 4 || fields.first() != Some(&"m=application") || fields.get(2) != Some(&"UDP/DTLS/SCTP") {
        return Err(incompatible("sdp: the offer's section is not a data channel"));
    }
    let candidate = section
        .iter()
        .filter_map(|l| l.strip_prefix("a=candidate:"))
        .map(|c| c.split(' ').collect::<Vec<_>>())
        .find(|f| {
            f.get(1) == Some(&"1")
                && f.get(2).is_some_and(|t| t.eq_ignore_ascii_case("udp"))
                && f.get(4).is_some_and(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
                && f.get(6) == Some(&"typ")
                && f.get(7) == Some(&"host")
        })
        .ok_or_else(|| incompatible("sdp: the offer has no IPv4 host candidate"))?;
    let (ip, port) = (candidate.get(4).copied().unwrap_or_default(), candidate.get(5).copied().unwrap_or_default());
    let mut fingerprints = Vec::new();
    let mut out = Vec::new();
    for line in &session {
        match *line {
            l if l.starts_with("a=fingerprint:") => fingerprints.push(l.to_string()),
            l if l.starts_with("a=msid-semantic") || l.starts_with("a=extmap-allow-mixed") || l.starts_with("a=ice-options:") => {}
            l if ["v=", "o=", "t=", "a=group:BUNDLE ", "a=ice-ufrag:", "a=ice-pwd:", "a=setup:", "a=ice-lite"].iter().any(|p| l.starts_with(p)) => {
                out.push(l.to_string());
            }
            "s=-" => {
                out.push("s=-".to_string());
                out.push("b=CT:4000".to_string());
            }
            l if l.starts_with("c=") || l.starts_with("b=") => {}
            l => return Err(unknown(l)),
        }
    }
    out.push("a=msid-semantic: WMS *".to_string());
    out.extend([
        format!("m=x-data {port} RTP/SAVP 127 126"),
        format!("c=IN IP4 {ip}"),
        "a=ice-options:trickle".to_string(),
        format!("a=candidate:{}", candidate.iter().take(8).copied().collect::<Vec<_>>().join(" ")),
        "a=rtpmap:127 x-data/90000".to_string(),
        "a=rtpmap:126 rtx/90000".to_string(),
        "a=fmtp:126 apt=127".to_string(),
        "a=x-data-protocol:sctp".to_string(),
        format!("a=x-ssrc-range:{ssrc}-{ssrc}"),
        "a=rtcp-mux".to_string(),
    ]);
    let mut has_fingerprint = false;
    for line in &section {
        match *line {
            l if l.starts_with("a=candidate:") || l == "a=end-of-candidates" || l.starts_with("a=ice-options:") || l.starts_with("c=") => {}
            l if l.starts_with("a=max-message-size:") => out.push("a=max-message-size:262144".to_string()),
            l if l.starts_with("a=fingerprint:") => {
                has_fingerprint = true;
                out.push(l.to_string());
            }
            l if ["a=setup:", "a=mid:", "a=sctp-port:", "a=ice-ufrag:", "a=ice-pwd:", "a=sendrecv", "a=rtcp-mux"]
                .iter()
                .any(|p| l.starts_with(p)) =>
            {
                if l != "a=rtcp-mux" {
                    out.push(l.to_string());
                }
            }
            l => return Err(unknown(l)),
        }
    }
    out.push("a=label:data".to_string());
    if !has_fingerprint {
        out.extend(fingerprints);
    }
    Ok(out.join("\r\n") + "\r\n")
}

/// Teams' answer as standard SDP for the host (the client's `Xz`): its one `m=x-data <port>
/// RTP/SAVP` section becomes `m=application <port> UDP/DTLS/SCTP webrtc-datachannel`, its rtp,
/// fmtp, rtcp, x- and label lines go, and it gets an `a=sctp-port`. A section with port 0 (the
/// service refused the data channel), an audio or video section, or an unknown line is an error.
pub fn from_ms(answer: &str) -> Result<String, ProtoError> {
    let lines = lines(answer)?;
    let (session, media, section) = one_section(&lines)?;
    let fields: Vec<&str> = media.split(' ').collect();
    let port = match (fields.first(), fields.get(1), fields.get(2)) {
        (Some(&"m=x-data"), Some(port), Some(&"RTP/SAVP")) => *port,
        _ => return Err(incompatible("sdp: the answer's section is not the data channel")),
    };
    if !port.parse::<u16>().is_ok_and(|p| p != 0) {
        return Err(incompatible("sdp: the answer refused the data channel"));
    }
    let mut out = Vec::new();
    for line in &session {
        match *line {
            l if [
                "v=",
                "o=",
                "s=",
                "t=",
                "c=",
                "b=",
                "a=group:BUNDLE ",
                "a=ice-ufrag:",
                "a=ice-pwd:",
                "a=fingerprint:",
                "a=setup:",
                "a=ice-lite",
            ]
            .iter()
            .any(|p| l.starts_with(p)) =>
            {
                out.push(l.to_string());
            }
            l if l.starts_with("a=msid-semantic") || l.starts_with("a=ice-options:") || l.starts_with("a=x-") => {}
            l => return Err(unknown(l)),
        }
    }
    out.push(format!("m=application {port} UDP/DTLS/SCTP webrtc-datachannel"));
    let mut has_port = false;
    for line in &section {
        match *line {
            l if ["a=rtpmap:", "a=fmtp:", "a=rtcp:", "a=rtcp-fb:", "a=x-", "a=label:", "a=crypto:", "a=ice-options:"]
                .iter()
                .any(|p| l.starts_with(p)) => {}
            l if [
                "c=",
                "b=",
                "a=candidate:",
                "a=end-of-candidates",
                "a=ice-ufrag:",
                "a=ice-pwd:",
                "a=fingerprint:",
                "a=setup:",
                "a=mid:",
                "a=rtcp-mux",
                "a=sendrecv",
                "a=sctp-port:",
                "a=max-message-size:",
            ]
            .iter()
            .any(|p| l.starts_with(p)) =>
            {
                has_port |= l.starts_with("a=sctp-port:");
                out.push(l.to_string());
            }
            l => return Err(unknown(l)),
        }
    }
    if !has_port {
        out.push(SCTP_PORT.to_string());
    }
    Ok(out.join("\r\n") + "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data-channel offer as webrtc-rs makes it, with documentation addresses and invented keys.
    const OFFER: &str = "v=0\r\no=- 1234 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
        a=fingerprint:sha-256 AA:BB:CC\r\na=group:BUNDLE 0\r\na=extmap-allow-mixed\r\na=msid-semantic: WMS\r\n\
        m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\na=setup:actpass\r\na=mid:0\r\n\
        a=sendrecv\r\na=sctp-port:5000\r\na=max-message-size:1073741823\r\na=ice-ufrag:ufrag\r\na=ice-pwd:password\r\n\
        a=candidate:1 1 udp 2130706431 2001:db8::1 50001 typ host\r\n\
        a=candidate:2 1 udp 2130706431 192.0.2.10 50000 typ host generation 0 network-id 1\r\n\
        a=candidate:3 1 udp 2130706431 192.0.2.11 50002 typ host\r\na=end-of-candidates\r\n";

    #[test]
    fn the_offer_becomes_the_web_clients_data_section() {
        let ms = to_ms(OFFER, 7).unwrap();
        let expected = [
            "v=0",
            "o=- 1234 1 IN IP4 0.0.0.0",
            "s=-",
            "b=CT:4000",
            "t=0 0",
            "a=group:BUNDLE 0",
            "a=msid-semantic: WMS *",
            "m=x-data 50000 RTP/SAVP 127 126",
            "c=IN IP4 192.0.2.10",
            "a=ice-options:trickle",
            "a=candidate:2 1 udp 2130706431 192.0.2.10 50000 typ host",
            "a=rtpmap:127 x-data/90000",
            "a=rtpmap:126 rtx/90000",
            "a=fmtp:126 apt=127",
            "a=x-data-protocol:sctp",
            "a=x-ssrc-range:7-7",
            "a=rtcp-mux",
            "a=setup:actpass",
            "a=mid:0",
            "a=sendrecv",
            "a=sctp-port:5000",
            "a=max-message-size:262144",
            "a=ice-ufrag:ufrag",
            "a=ice-pwd:password",
            "a=label:data",
            "a=fingerprint:sha-256 AA:BB:CC",
        ];
        assert_eq!(ms, expected.join("\r\n") + "\r\n");
    }

    #[test]
    fn an_offer_with_more_or_other_is_refused() {
        let audio = format!("{OFFER}m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\n");
        assert!(to_ms(&audio, 1).is_err());
        assert!(to_ms(&OFFER.replace("m=application 9 UDP/DTLS/SCTP", "m=audio 9 UDP/TLS/RTP/SAVPF"), 1).is_err());
        assert!(to_ms(&OFFER.replace("a=sendrecv", "a=ssrc:1 cname:x"), 1).is_err());
        let no_v4 = OFFER.replace("192.0.2.10", "2001:db8::2").replace("192.0.2.11", "2001:db8::3");
        assert!(to_ms(&no_v4, 1).is_err());
        assert!(to_ms(&format!("{OFFER}a=x:{}\r\n", "y".repeat(MAX_LINE)), 1).is_err());
        assert!(to_ms("", 1).is_err());
    }

    /// Teams' answer to a data-only offer (docs, "Live results" 18), with documentation-style
    /// addresses in its media range and invented keys.
    const ANSWER: &str = "v=0\r\no=- 0 0 IN IP4 52.112.0.10\r\ns=session\r\nc=IN IP4 52.112.0.10\r\nb=CT:10000000\r\nt=0 0\r\n\
        a=x-mediabw:data-video send=4000;recv=4000\r\nm=x-data 3480 RTP/SAVP 127 126\r\nc=IN IP4 52.112.0.10\r\n\
        a=rtpmap:127 x-data/90000\r\na=rtpmap:126 rtx/90000\r\na=fmtp:126 apt=127\r\na=rtcp:3480\r\n\
        a=ice-ufrag:ufrag\r\na=ice-pwd:password\r\na=fingerprint:sha-256 DD:EE:FF\r\na=setup:passive\r\n\
        a=candidate:1 1 UDP 2130706431 52.112.0.10 3480 typ relay raddr 52.112.0.10 rport 3480 MTURNID 1\r\n\
        a=candidate:2 1 tcp-pass 174455807 52.112.0.11 3480 typ relay raddr 52.112.0.11 rport 3480\r\n\
        a=x-source-streamid:1\r\na=x-ssrc-range:100-100\r\na=x-data-protocol:sctp\r\na=label:data\r\na=rtcp-mux\r\n\
        a=mid:0\r\na=sendrecv\r\n";

    #[test]
    fn the_answer_becomes_a_standard_data_section() {
        let plain = from_ms(ANSWER).unwrap();
        let expected = [
            "v=0",
            "o=- 0 0 IN IP4 52.112.0.10",
            "s=session",
            "c=IN IP4 52.112.0.10",
            "b=CT:10000000",
            "t=0 0",
            "m=application 3480 UDP/DTLS/SCTP webrtc-datachannel",
            "c=IN IP4 52.112.0.10",
            "a=ice-ufrag:ufrag",
            "a=ice-pwd:password",
            "a=fingerprint:sha-256 DD:EE:FF",
            "a=setup:passive",
            "a=candidate:1 1 UDP 2130706431 52.112.0.10 3480 typ relay raddr 52.112.0.10 rport 3480 MTURNID 1",
            "a=candidate:2 1 tcp-pass 174455807 52.112.0.11 3480 typ relay raddr 52.112.0.11 rport 3480",
            "a=rtcp-mux",
            "a=mid:0",
            "a=sendrecv",
            "a=sctp-port:5000",
        ];
        assert_eq!(plain, expected.join("\r\n") + "\r\n");
    }

    #[test]
    fn an_answer_with_audio_a_refusal_or_unknown_lines_is_an_error() {
        let audio = ANSWER.replace("m=x-data 3480", "m=audio 3478 RTP/SAVP 111\r\na=recvonly\r\nm=x-data 3480");
        assert!(from_ms(&audio).is_err());
        assert!(from_ms(&ANSWER.replace("m=x-data 3480", "m=x-data 0")).is_err());
        assert!(from_ms(&ANSWER.replace("m=x-data 3480", "m=audio 3480")).is_err());
        let ssrc = ANSWER.replace("a=sendrecv", "a=ssrc:1 cname:x");
        assert_eq!(from_ms(&ssrc), Err(incompatible("sdp: unexpected line a=ssrc")));
        assert!(from_ms(&"a=x-y\r\n".repeat(MAX_LINES + 1)).is_err());
        assert!(from_ms(&format!("{ANSWER}{}", "a=x-y\r\n".repeat(MAX_SDP / 6))).is_err());
        assert!(from_ms("v=0\r\n").is_err());
    }
}
