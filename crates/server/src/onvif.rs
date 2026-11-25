//! Minimal ONVIF Profile S device: WS-Discovery plus the Device and Media (ver10) calls an NVR needs to find
//! the camera, pick its streams (main and sub), audio and snapshot. Authentication headers are ignored
//! (local network only).

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wts_common::{AUDIO_RATE, Camera, H264_PROFILES, RTSP_PORT, Stream, size, streams, version};

pub struct Ctx {
    pub ip: String,
    pub port: u16,
    pub camera: Camera,
    pub hostname: String,
}

const NS: &str = r#"xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:ter="http://www.onvif.org/ver10/error""#;
const MULTICAST: &str = "<tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address><tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast><tt:SessionTimeout>PT60S</tt:SessionTimeout>";
const MCAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const WSD_PORT: u16 = 3702;

/// MAC address of the network interface (wireless/ethernet first), 0 if none can be read.
pub static MAC: LazyLock<u64> = LazyLock::new(|| {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort_by_key(|n| (!(n.starts_with("wl") || n.starts_with("en") || n.starts_with("eth")), n.clone()));
    names.iter()
        .filter(|n| *n != "lo")
        .filter_map(|n| std::fs::read_to_string(format!("/sys/class/net/{n}/address")).ok())
        .filter_map(|a| u64::from_str_radix(&a.trim().replace(':', ""), 16).ok())
        .find(|&m| m != 0)
        .unwrap_or(0)
});

/// Same id as the Python versions (uuid5 of the MAC), so an NVR keeps recognising the camera.
pub static DEVICE_UUID: LazyLock<uuid::Uuid> =
    LazyLock::new(|| uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, format!("walltablet-{:012x}", *MAC).as_bytes()));

static INSTANCE: LazyLock<u64> =
    LazyLock::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_secs()));
static MESSAGE_NUMBER: AtomicU64 = AtomicU64::new(1);

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn url_quote(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn envelope(body: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<s:Envelope {NS}><s:Body>{body}</s:Body></s:Envelope>")
}

fn fault(code: &str, sub: &str, text: &str) -> String {
    envelope(&format!(
        "<s:Fault><s:Code><s:Value>s:{code}</s:Value><s:Subcode><s:Value>ter:{sub}</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang=\"en\">{}</s:Text></s:Reason></s:Fault>",
        escape(text)
    ))
}

fn mac_str() -> String {
    (0..6).rev().map(|i| format!("{:02x}", (*MAC >> (i * 8)) & 0xff)).collect::<Vec<_>>().join(":")
}

fn scopes(c: &Camera) -> String {
    format!(
        "onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/type/audio_encoder onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/name/{} onvif://www.onvif.org/hardware/WallTablet",
        url_quote(&c.name)
    )
}

fn video_source_cfg(c: &Camera) -> String {
    let (w, h) = size(c, false);
    format!(
        "<tt:Name>vsc</tt:Name><tt:UseCount>{}</tt:UseCount><tt:SourceToken>vs</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/>",
        streams(c).len()
    )
}

fn h264_profile(c: &Camera) -> &'static str {
    H264_PROFILES.iter().find(|p| p.0 == c.profile).map_or("Main", |p| p.1)
}

fn encoder_cfg(c: &Camera, s: &Stream) -> String {
    format!(
        "<tt:Name>{}</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:Resolution><tt:Quality>5</tt:Quality><tt:RateControl><tt:FrameRateLimit>{}</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{}</tt:BitrateLimit></tt:RateControl><tt:H264><tt:GovLength>{}</tt:GovLength><tt:H264Profile>{}</tt:H264Profile></tt:H264>{MULTICAST}",
        s.encoder, s.width, s.height, c.fps, s.kbit, c.fps * c.gop, h264_profile(c)
    )
}

const AUDIO_SOURCE_CFG: &str = "<tt:Name>asc</tt:Name><tt:UseCount>1</tt:UseCount><tt:SourceToken>as</tt:SourceToken>";

fn audio_encoder_cfg() -> String {
    format!(
        "<tt:Name>aec</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>AAC</tt:Encoding><tt:Bitrate>64</tt:Bitrate><tt:SampleRate>{}</tt:SampleRate>{MULTICAST}",
        AUDIO_RATE / 1000
    )
}

/// Children in the order the ONVIF schema requires: Name, VideoSource, AudioSource, VideoEncoder, AudioEncoder.
fn profile(c: &Camera, tag: &str, s: &Stream) -> String {
    let audio = c.audio_enabled && s.profile == "main"; // audio rides on the main stream only
    let mut x = format!(
        "<trt:{tag} token=\"{p}\" fixed=\"true\"><tt:Name>{p}</tt:Name><tt:VideoSourceConfiguration token=\"vsc\">{}</tt:VideoSourceConfiguration>",
        video_source_cfg(c),
        p = s.profile
    );
    if audio {
        x += &format!("<tt:AudioSourceConfiguration token=\"asc\">{AUDIO_SOURCE_CFG}</tt:AudioSourceConfiguration>");
    }
    x += &format!("<tt:VideoEncoderConfiguration token=\"{}\">{}</tt:VideoEncoderConfiguration>", s.encoder, encoder_cfg(c, s));
    if audio {
        x += &format!("<tt:AudioEncoderConfiguration token=\"aec\">{}</tt:AudioEncoderConfiguration>", audio_encoder_cfg());
    }
    x + &format!("</trt:{tag}>")
}

fn media_uri(uri: &str) -> String {
    format!(
        "<trt:MediaUri><tt:Uri>{}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri>",
        escape(uri)
    )
}

/// Text of the first element with this local name inside the request, e.g. ProfileToken.
fn arg(req: roxmltree::Node, name: &str) -> Option<String> {
    req.descendants().find(|n| n.is_element() && n.tag_name().name() == name).and_then(|n| n.text()).map(|t| t.trim().to_string())
}

fn utc_now() -> (u64, u64, u64, i64, u64, u64) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil-from-days (Howard Hinnant), proleptic Gregorian calendar
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (rem / 3600, rem / 60 % 60, rem % 60, year, month, day)
}

fn responses(action: &str, ctx: &Ctx, req: roxmltree::Node) -> Option<String> {
    let c = &ctx.camera;
    let base = format!("http://{}:{}/onvif", ctx.ip, ctx.port);
    let version_xml = "<tt:Major>2</tt:Major><tt:Minor>5</tt:Minor>";
    let all = streams(c);
    // unknown or missing tokens get the main stream: lenient for NVRs that skip them
    let profile_token = arg(req, "ProfileToken");
    let encoder_token = arg(req, "ConfigurationToken");
    let by_profile = all.iter().find(|s| Some(s.profile) == profile_token.as_deref()).unwrap_or(&all[0]);
    let by_encoder = all.iter().find(|s| Some(s.encoder) == encoder_token.as_deref()).unwrap_or(&all[0]);
    let x = match action {
        "GetSystemDateAndTime" => {
            let (h, mi, s, y, mo, d) = utc_now();
            format!("<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>NTP</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings><tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone><tt:UTCDateTime><tt:Time><tt:Hour>{h}</tt:Hour><tt:Minute>{mi}</tt:Minute><tt:Second>{s}</tt:Second></tt:Time><tt:Date><tt:Year>{y}</tt:Year><tt:Month>{mo}</tt:Month><tt:Day>{d}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>")
        }
        "GetDeviceInformation" => format!(
            "<tds:GetDeviceInformationResponse><tds:Manufacturer>WallTabletSuite</tds:Manufacturer><tds:Model>{}</tds:Model><tds:FirmwareVersion>{}</tds:FirmwareVersion><tds:SerialNumber>{:012X}</tds:SerialNumber><tds:HardwareId>WallTablet</tds:HardwareId></tds:GetDeviceInformationResponse>",
            escape(&c.name), version(), *MAC
        ),
        "GetCapabilities" => format!(
            "<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Device><tt:XAddr>{base}/device_service</tt:XAddr></tt:Device><tt:Media><tt:XAddr>{base}/media_service</tt:XAddr><tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media></tds:Capabilities></tds:GetCapabilitiesResponse>"
        ),
        "GetServices" => {
            let svc: String = ["device", "media"].iter().map(|ns| format!(
                "<tds:Service><tds:Namespace>http://www.onvif.org/ver10/{ns}/wsdl</tds:Namespace><tds:XAddr>{base}/{ns}_service</tds:XAddr><tds:Version>{version_xml}</tds:Version></tds:Service>"
            )).collect();
            format!("<tds:GetServicesResponse>{svc}</tds:GetServicesResponse>")
        }
        "GetScopes" => {
            let items: String = scopes(c).split(' ').map(|s| format!(
                "<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>{}</tt:ScopeItem></tds:Scopes>", escape(s)
            )).collect();
            format!("<tds:GetScopesResponse>{items}</tds:GetScopesResponse>")
        }
        "GetHostname" => format!(
            "<tds:GetHostnameResponse><tds:HostnameInformation><tt:FromDHCP>false</tt:FromDHCP><tt:Name>{}</tt:Name></tds:HostnameInformation></tds:GetHostnameResponse>",
            escape(&ctx.hostname)
        ),
        "GetNetworkInterfaces" => format!(
            "<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token=\"eth0\"><tt:Enabled>true</tt:Enabled><tt:Info><tt:Name>eth0</tt:Name><tt:HwAddress>{}</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info><tt:IPv4><tt:Enabled>true</tt:Enabled><tt:Config><tt:FromDHCP><tt:Address>{}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:FromDHCP><tt:DHCP>true</tt:DHCP></tt:Config></tt:IPv4></tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>",
            mac_str(), escape(&ctx.ip)
        ),
        "GetProfiles" => format!(
            "<trt:GetProfilesResponse>{}</trt:GetProfilesResponse>",
            all.iter().map(|s| profile(c, "Profiles", s)).collect::<String>()
        ),
        "GetProfile" => format!("<trt:GetProfileResponse>{}</trt:GetProfileResponse>", profile(c, "Profile", by_profile)),
        "GetStreamUri" => format!(
            "<trt:GetStreamUriResponse>{}</trt:GetStreamUriResponse>",
            media_uri(&format!("rtsp://{}:{RTSP_PORT}/{}", ctx.ip, by_profile.path))
        ),
        "GetSnapshotUri" => format!(
            "<trt:GetSnapshotUriResponse>{}</trt:GetSnapshotUriResponse>",
            media_uri(&format!("http://{}:{}/snapshot.jpg", ctx.ip, ctx.port))
        ),
        "GetVideoSources" => {
            let (w, h) = size(c, false);
            format!("<trt:GetVideoSourcesResponse><trt:VideoSources token=\"vs\"><tt:Framerate>{}</tt:Framerate><tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution></trt:VideoSources></trt:GetVideoSourcesResponse>", c.fps)
        }
        "GetVideoSourceConfigurations" => format!(
            "<trt:GetVideoSourceConfigurationsResponse><trt:Configurations token=\"vsc\">{}</trt:Configurations></trt:GetVideoSourceConfigurationsResponse>",
            video_source_cfg(c)
        ),
        "GetVideoEncoderConfigurations" => format!(
            "<trt:GetVideoEncoderConfigurationsResponse>{}</trt:GetVideoEncoderConfigurationsResponse>",
            all.iter().map(|s| format!("<trt:Configurations token=\"{}\">{}</trt:Configurations>", s.encoder, encoder_cfg(c, s))).collect::<String>()
        ),
        "GetVideoEncoderConfiguration" => format!(
            "<trt:GetVideoEncoderConfigurationResponse><trt:Configuration token=\"{}\">{}</trt:Configuration></trt:GetVideoEncoderConfigurationResponse>",
            by_encoder.encoder, encoder_cfg(c, by_encoder)
        ),
        "GetVideoEncoderConfigurationOptions" => {
            let s = if encoder_token.is_some() { by_encoder } else { by_profile };
            format!("<trt:GetVideoEncoderConfigurationOptionsResponse><trt:Options><tt:QualityRange><tt:Min>0</tt:Min><tt:Max>10</tt:Max></tt:QualityRange><tt:H264><tt:ResolutionsAvailable><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:ResolutionsAvailable><tt:GovLengthRange><tt:Min>1</tt:Min><tt:Max>600</tt:Max></tt:GovLengthRange><tt:FrameRateRange><tt:Min>1</tt:Min><tt:Max>60</tt:Max></tt:FrameRateRange><tt:EncodingIntervalRange><tt:Min>1</tt:Min><tt:Max>1</tt:Max></tt:EncodingIntervalRange><tt:H264ProfilesSupported>{}</tt:H264ProfilesSupported></tt:H264></trt:Options></trt:GetVideoEncoderConfigurationOptionsResponse>", s.width, s.height, h264_profile(c))
        }
        "GetAudioSources" => format!(
            "<trt:GetAudioSourcesResponse>{}</trt:GetAudioSourcesResponse>",
            if c.audio_enabled { "<trt:AudioSources token=\"as\"><tt:Channels>1</tt:Channels></trt:AudioSources>" } else { "" }
        ),
        "GetAudioSourceConfigurations" => format!(
            "<trt:GetAudioSourceConfigurationsResponse>{}</trt:GetAudioSourceConfigurationsResponse>",
            if c.audio_enabled { format!("<trt:Configurations token=\"asc\">{AUDIO_SOURCE_CFG}</trt:Configurations>") } else { String::new() }
        ),
        "GetAudioEncoderConfigurations" => format!(
            "<trt:GetAudioEncoderConfigurationsResponse>{}</trt:GetAudioEncoderConfigurationsResponse>",
            if c.audio_enabled { format!("<trt:Configurations token=\"aec\">{}</trt:Configurations>", audio_encoder_cfg()) } else { String::new() }
        ),
        _ => return None,
    };
    Some(x)
}

/// SOAP request bytes -> (HTTP status, response XML).
pub fn handle(body: &[u8], ctx: &Ctx) -> (u16, String) {
    if !ctx.camera.enabled {
        return (503, fault("Receiver", "ActionNotSupported", "camera disabled"));
    }
    let Ok(text) = std::str::from_utf8(body) else { return (400, fault("Sender", "WellFormed", "not UTF-8")) };
    let Ok(doc) = roxmltree::Document::parse(text) else { return (400, fault("Sender", "WellFormed", "not a SOAP request")) };
    let Some(req) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Body")
        .and_then(|b| b.children().find(|n| n.is_element()))
    else {
        return (400, fault("Sender", "WellFormed", "not a SOAP request"));
    };
    let action = req.tag_name().name();
    match responses(action, ctx, req) {
        Some(x) => (200, envelope(&x)),
        None => (500, fault("Receiver", "ActionNotSupported", &format!("{action} not supported"))),
    }
}

/// WS-Discovery Probe bytes -> ProbeMatches bytes, or None if it is not a Probe.
pub fn probe_match(probe: &[u8], ctx: &Ctx) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(probe).ok()?;
    let doc = roxmltree::Document::parse(text).ok()?;
    let body = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == "Body")?;
    body.children().find(|n| n.is_element() && n.tag_name().name() == "Probe")?;
    let relates = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "MessageID")
        .and_then(|n| n.text())
        .map(|t| escape(t.trim()))
        .unwrap_or_default();
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\" xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"><s:Header><a:MessageID>uuid:{}</a:MessageID><a:RelatesTo>{relates}</a:RelatesTo><a:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To><a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action><d:AppSequence InstanceId=\"{}\" MessageNumber=\"{}\"/></s:Header><s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:{}</a:Address></a:EndpointReference><d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types><d:Scopes>{}</d:Scopes><d:XAddrs>http://{}:{}/onvif/device_service</d:XAddrs><d:MetadataVersion>1</d:MetadataVersion></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>",
        uuid::Uuid::new_v4(), *INSTANCE, MESSAGE_NUMBER.fetch_add(1, Ordering::Relaxed), *DEVICE_UUID,
        escape(&scopes(&ctx.camera)), ctx.ip, ctx.port
    ).into_bytes())
}

/// Answers WS-Discovery probes while the camera is enabled. `ctx_for(ip)` builds the context for our address.
pub fn discovery(ctx_for: impl Fn(String) -> Ctx) {
    let socket = loop {
        match bind_wsd() {
            Ok(s) => break s,
            Err(e) => {
                println!("WS-Discovery: {e}, neuer Versuch in 10 s");
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    };
    let mut buf = vec![0u8; 65535];
    let mut joined = false;
    loop {
        if !joined {
            // re-joined now and then: a WLAN reconnect can drop the membership
            let _ = socket.leave_multicast_v4(&MCAST, &Ipv4Addr::UNSPECIFIED);
            joined = socket.join_multicast_v4(&MCAST, &Ipv4Addr::UNSPECIFIED).is_ok();
            if !joined {
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let Some(ip) = crate::local_ip_for(from.ip()) else { continue };
                let ctx = ctx_for(ip);
                if ctx.camera.enabled
                    && let Some(reply) = probe_match(&buf[..n], &ctx) {
                        let _ = socket.send_to(&reply, from);
                    }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => joined = false,
            Err(e) => {
                println!("WS-Discovery: {e}");
                joined = false;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn bind_wsd() -> std::io::Result<UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, WSD_PORT)).into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_secs(300)))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx { ip: "10.0.0.5".into(), port: 80, camera: Camera { enabled: true, ..Camera::default() }, hostname: "tab".into() }
    }

    fn soap(action: &str, inner: &str, ctx: &Ctx) -> (u16, String) {
        let body = format!("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><{action} xmlns=\"http://www.onvif.org/ver10/media/wsdl\">{inner}</{action}></s:Body></s:Envelope>");
        handle(body.as_bytes(), ctx)
    }

    fn texts(xml: &str, name: &str) -> Vec<String> {
        let doc = roxmltree::Document::parse(xml).unwrap();
        doc.descendants().filter(|n| n.tag_name().name() == name).filter_map(|n| n.text().map(str::to_string)).collect()
    }

    #[test]
    fn every_action_is_well_formed() {
        let mut c = ctx();
        c.camera.audio_enabled = true;
        for a in ["GetSystemDateAndTime", "GetDeviceInformation", "GetCapabilities", "GetServices", "GetScopes", "GetHostname",
                  "GetNetworkInterfaces", "GetProfiles", "GetProfile", "GetStreamUri", "GetSnapshotUri", "GetVideoSources",
                  "GetVideoSourceConfigurations", "GetVideoEncoderConfigurations", "GetVideoEncoderConfiguration",
                  "GetVideoEncoderConfigurationOptions", "GetAudioSources", "GetAudioSourceConfigurations",
                  "GetAudioEncoderConfigurations"] {
            let (code, xml) = soap(a, "", &c);
            assert_eq!(code, 200, "{a}");
            let doc = roxmltree::Document::parse(&xml).unwrap_or_else(|e| panic!("{a}: {e}"));
            assert!(doc.descendants().any(|n| n.tag_name().name() == format!("{a}Response")), "{a}");
        }
        let (code, xml) = soap("SetHostname", "", &c);
        assert_eq!(code, 500);
        assert!(xml.contains("ActionNotSupported"));
        assert_eq!(handle(b"not xml", &c).0, 400);
        c.camera.enabled = false;
        assert_eq!(soap("GetProfiles", "", &c).0, 503);
    }

    #[test]
    fn streams_rotation_audio_snapshot() {
        let mut c = ctx();
        c.camera.rotate = "90".into();
        c.camera.audio_enabled = true;
        let (_, xml) = soap("GetProfiles", "", &c);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let tokens: Vec<_> = doc.descendants().filter(|n| n.tag_name().name() == "Profiles").filter_map(|n| n.attribute("token")).collect();
        assert_eq!(tokens, ["main", "sub"]);
        assert_eq!(texts(&xml, "Width")[0], "720"); // rotated: width and height swapped
        assert_eq!(texts(&xml, "Encoding").iter().filter(|e| *e == "AAC").count(), 1);
        // schema order inside a profile: VideoSource, AudioSource, VideoEncoder, AudioEncoder
        let main = doc.descendants().find(|n| n.attribute("token") == Some("main")).unwrap();
        let order: Vec<_> = main.children().filter(|n| n.is_element()).map(|n| n.tag_name().name()).collect();
        assert_eq!(order, ["Name", "VideoSourceConfiguration", "AudioSourceConfiguration", "VideoEncoderConfiguration", "AudioEncoderConfiguration"]);
        assert!(texts(&soap("GetStreamUri", "<ProfileToken>sub</ProfileToken>", &c).1, "Uri")[0].ends_with(":8554/sub"));
        assert!(texts(&soap("GetStreamUri", "<ProfileToken>x</ProfileToken>", &c).1, "Uri")[0].ends_with(":8554/stream"));
        assert_eq!(texts(&soap("GetSnapshotUri", "", &c).1, "Uri")[0], "http://10.0.0.5:80/snapshot.jpg");
        assert_eq!(texts(&soap("GetVideoEncoderConfiguration", "<ConfigurationToken>vec_sub</ConfigurationToken>", &c).1, "Width"), ["360"]);
        c.camera.sub_enabled = false;
        c.camera.audio_enabled = false;
        let (_, xml) = soap("GetProfiles", "", &c);
        assert_eq!(texts(&xml, "Name").iter().filter(|n| *n == "main" || *n == "sub").count(), 1);
        assert!(!xml.contains("AAC"));
    }

    #[test]
    fn discovery_probe() {
        let c = ctx();
        for probe in [
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\"><s:Header><a:MessageID>uuid:1234</a:MessageID></s:Header><s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>",
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\"><s:Header><a:MessageID>uuid:1234</a:MessageID></s:Header><s:Body><d:Probe/></s:Body></s:Envelope>",
        ] {
            let reply = String::from_utf8(probe_match(probe.as_bytes(), &c).unwrap()).unwrap();
            assert_eq!(texts(&reply, "RelatesTo"), ["uuid:1234"]);
            assert_eq!(texts(&reply, "XAddrs"), ["http://10.0.0.5:80/onvif/device_service"]);
            assert!(reply.contains("AppSequence"));
            assert!(probe_match(reply.as_bytes(), &c).is_none()); // own ProbeMatches is no Probe
        }
        assert!(probe_match(b"garbage", &c).is_none());
    }

    #[test]
    fn date_is_sane() {
        let (h, mi, s, y, mo, d) = utc_now();
        assert!(h < 24 && mi < 60 && s < 60 && (2024..2200).contains(&y) && (1..=12).contains(&mo) && (1..=31).contains(&d));
    }
}
