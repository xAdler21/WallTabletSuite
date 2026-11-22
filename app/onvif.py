"""Minimal ONVIF Profile S device: WS-Discovery plus the Device and Media (ver10) calls an NVR needs
to find the camera and fetch its RTSP stream. Authentication headers are ignored (local network only).
"""
import re
import socket
import struct
import time
import uuid
import xml.etree.ElementTree as ET
from urllib.parse import quote
from xml.sax.saxutils import escape

MAC = uuid.getnode()
DEVICE_UUID = uuid.uuid5(uuid.NAMESPACE_DNS, f"walltablet-{MAC:012x}")
MCAST = ("239.255.255.250", 3702)

NS = ('xmlns:s="http://www.w3.org/2003/05/soap-envelope" '
      'xmlns:tds="http://www.onvif.org/ver10/device/wsdl" '
      'xmlns:trt="http://www.onvif.org/ver10/media/wsdl" '
      'xmlns:tt="http://www.onvif.org/ver10/schema" '
      'xmlns:ter="http://www.onvif.org/ver10/error"')


def envelope(body):
    return f'<?xml version="1.0" encoding="UTF-8"?>\n<s:Envelope {NS}><s:Body>{body}</s:Body></s:Envelope>'


def fault(code, sub, text):
    return envelope(f'<s:Fault><s:Code><s:Value>s:{code}</s:Value><s:Subcode><s:Value>ter:{sub}</s:Value>'
                    f'</s:Subcode></s:Code><s:Reason><s:Text xml:lang="en">{escape(text)}</s:Text></s:Reason></s:Fault>')


def mac_str():
    return ":".join(f"{(MAC >> s) & 0xff:02x}" for s in range(40, -8, -8))


def scopes(ctx):
    name = quote(ctx["camera"]["name"])
    return ("onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/Profile/Streaming "
            f"onvif://www.onvif.org/name/{name} onvif://www.onvif.org/hardware/WallTablet")


def video_source_cfg(c):
    return (f'<tt:Name>vsc</tt:Name><tt:UseCount>1</tt:UseCount><tt:SourceToken>vs</tt:SourceToken>'
            f'<tt:Bounds x="0" y="0" width="{c["width"]}" height="{c["height"]}"/>')


def encoder_cfg(c):
    return (f'<tt:Name>vec</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H264</tt:Encoding>'
            f'<tt:Resolution><tt:Width>{c["width"]}</tt:Width><tt:Height>{c["height"]}</tt:Height></tt:Resolution>'
            f'<tt:Quality>5</tt:Quality><tt:RateControl><tt:FrameRateLimit>{c["fps"]}</tt:FrameRateLimit>'
            f'<tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{c["bitrate"]}</tt:BitrateLimit></tt:RateControl>'
            f'<tt:H264><tt:GovLength>{c["fps"] * 2}</tt:GovLength><tt:H264Profile>Main</tt:H264Profile></tt:H264>'
            '<tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address>'
            '<tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast>'
            '<tt:SessionTimeout>PT60S</tt:SessionTimeout>')


def profile(c, tag):
    return (f'<trt:{tag} token="main" fixed="true"><tt:Name>main</tt:Name>'
            f'<tt:VideoSourceConfiguration token="vsc">{video_source_cfg(c)}</tt:VideoSourceConfiguration>'
            f'<tt:VideoEncoderConfiguration token="vec">{encoder_cfg(c)}</tt:VideoEncoderConfiguration>'
            f'</trt:{tag}>')


def responses(action, ctx):
    c, base = ctx["camera"], f'http://{ctx["ip"]}:{ctx["port"]}/onvif'
    version = '<tt:Major>2</tt:Major><tt:Minor>5</tt:Minor>'
    if action == "GetSystemDateAndTime":
        t = time.gmtime()
        return ('<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>NTP</tt:DateTimeType>'
                '<tt:DaylightSavings>false</tt:DaylightSavings><tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>'
                f'<tt:UTCDateTime><tt:Time><tt:Hour>{t.tm_hour}</tt:Hour><tt:Minute>{t.tm_min}</tt:Minute>'
                f'<tt:Second>{t.tm_sec}</tt:Second></tt:Time><tt:Date><tt:Year>{t.tm_year}</tt:Year>'
                f'<tt:Month>{t.tm_mon}</tt:Month><tt:Day>{t.tm_mday}</tt:Day></tt:Date></tt:UTCDateTime>'
                '</tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>')
    if action == "GetDeviceInformation":
        return ('<tds:GetDeviceInformationResponse><tds:Manufacturer>WallTabletSuite</tds:Manufacturer>'
                f'<tds:Model>{escape(c["name"])}</tds:Model><tds:FirmwareVersion>{ctx["version"]}</tds:FirmwareVersion>'
                f'<tds:SerialNumber>{MAC:012X}</tds:SerialNumber><tds:HardwareId>WallTablet</tds:HardwareId>'
                '</tds:GetDeviceInformationResponse>')
    if action == "GetCapabilities":
        return ('<tds:GetCapabilitiesResponse><tds:Capabilities>'
                f'<tt:Device><tt:XAddr>{base}/device_service</tt:XAddr></tt:Device>'
                f'<tt:Media><tt:XAddr>{base}/media_service</tt:XAddr><tt:StreamingCapabilities>'
                '<tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP>'
                '<tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media>'
                '</tds:Capabilities></tds:GetCapabilitiesResponse>')
    if action == "GetServices":
        svc = ''.join(f'<tds:Service><tds:Namespace>http://www.onvif.org/ver10/{ns}/wsdl</tds:Namespace>'
                      f'<tds:XAddr>{base}/{ns}_service</tds:XAddr><tds:Version>{version}</tds:Version></tds:Service>'
                      for ns in ("device", "media"))
        return f'<tds:GetServicesResponse>{svc}</tds:GetServicesResponse>'
    if action == "GetScopes":
        items = ''.join(f'<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>{escape(s)}</tt:ScopeItem></tds:Scopes>'
                        for s in scopes(ctx).split())
        return f'<tds:GetScopesResponse>{items}</tds:GetScopesResponse>'
    if action == "GetHostname":
        return (f'<tds:GetHostnameResponse><tds:HostnameInformation><tt:FromDHCP>false</tt:FromDHCP>'
                f'<tt:Name>{escape(ctx["hostname"])}</tt:Name></tds:HostnameInformation></tds:GetHostnameResponse>')
    if action == "GetNetworkInterfaces":
        return ('<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token="eth0"><tt:Enabled>true</tt:Enabled>'
                f'<tt:Info><tt:Name>eth0</tt:Name><tt:HwAddress>{mac_str()}</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info>'
                '<tt:IPv4><tt:Enabled>true</tt:Enabled><tt:Config><tt:FromDHCP>'
                f'<tt:Address>{ctx["ip"]}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:FromDHCP>'
                '<tt:DHCP>true</tt:DHCP></tt:Config></tt:IPv4></tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>')
    if action == "GetProfiles":
        return f'<trt:GetProfilesResponse>{profile(c, "Profiles")}</trt:GetProfilesResponse>'
    if action == "GetProfile":
        return f'<trt:GetProfileResponse>{profile(c, "Profile")}</trt:GetProfileResponse>'
    if action == "GetStreamUri":
        return (f'<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>rtsp://{ctx["ip"]}:{ctx["rtsp_port"]}/stream</tt:Uri>'
                '<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>'
                '<tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetStreamUriResponse>')
    if action == "GetVideoSources":
        return (f'<trt:GetVideoSourcesResponse><trt:VideoSources token="vs"><tt:Framerate>{c["fps"]}</tt:Framerate>'
                f'<tt:Resolution><tt:Width>{c["width"]}</tt:Width><tt:Height>{c["height"]}</tt:Height></tt:Resolution>'
                '</trt:VideoSources></trt:GetVideoSourcesResponse>')
    if action == "GetVideoSourceConfigurations":
        return (f'<trt:GetVideoSourceConfigurationsResponse><trt:Configurations token="vsc">{video_source_cfg(c)}'
                '</trt:Configurations></trt:GetVideoSourceConfigurationsResponse>')
    if action == "GetVideoEncoderConfigurations":
        return (f'<trt:GetVideoEncoderConfigurationsResponse><trt:Configurations token="vec">{encoder_cfg(c)}'
                '</trt:Configurations></trt:GetVideoEncoderConfigurationsResponse>')
    if action == "GetVideoEncoderConfiguration":
        return (f'<trt:GetVideoEncoderConfigurationResponse><trt:Configuration token="vec">{encoder_cfg(c)}'
                '</trt:Configuration></trt:GetVideoEncoderConfigurationResponse>')
    return None


def handle(body, ctx):
    """SOAP request bytes -> (HTTP status, response XML)."""
    if not ctx["camera"]["enabled"]:
        return 503, fault("Receiver", "ActionNotSupported", "camera disabled")
    try:
        soap_body = next(e for e in ET.fromstring(body).iter() if e.tag.endswith("}Body"))
        action = list(soap_body)[0].tag.rsplit("}", 1)[-1]
    except (ET.ParseError, StopIteration, IndexError):
        return 400, fault("Sender", "WellFormed", "not a SOAP request")
    xml = responses(action, ctx)
    if xml is None:
        return 500, fault("Receiver", "ActionNotSupported", f"{action} not supported")
    return 200, envelope(xml)


def probe_match(probe, ctx):
    """WS-Discovery Probe bytes -> ProbeMatches bytes, or None if it is not a Probe."""
    text = probe.decode("utf-8", "replace")
    if not re.search(r"<(\w+:)?Probe[\s>]", text):
        return None
    m = re.search(r"MessageID[^>]*>\s*([^<\s]+)\s*<", text)
    relates = escape(m.group(1)) if m else ""
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" '
        'xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" '
        'xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" '
        'xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><s:Header>'
        f'<a:MessageID>uuid:{uuid.uuid4()}</a:MessageID><a:RelatesTo>{relates}</a:RelatesTo>'
        '<a:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To>'
        '<a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action>'
        '</s:Header><s:Body><d:ProbeMatches><d:ProbeMatch>'
        f'<a:EndpointReference><a:Address>urn:uuid:{DEVICE_UUID}</a:Address></a:EndpointReference>'
        '<d:Types>dn:NetworkVideoTransmitter</d:Types>'
        f'<d:Scopes>{escape(scopes(ctx))}</d:Scopes>'
        f'<d:XAddrs>http://{ctx["ip"]}:{ctx["port"]}/onvif/device_service</d:XAddrs>'
        '<d:MetadataVersion>1</d:MetadataVersion></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>'
    ).encode()


def local_ip_for(addr):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect((addr, 9))  # no packet is sent; picks the interface that routes to addr
        return s.getsockname()[0]
    finally:
        s.close()


def discovery(ctx_for_ip):
    """Answers WS-Discovery probes while the camera is enabled. ctx_for_ip(ip) -> ctx."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", MCAST[1]))
    sock.settimeout(300)
    mreq = struct.pack("4s4s", socket.inet_aton(MCAST[0]), socket.inet_aton("0.0.0.0"))
    joined = False
    while True:
        try:
            if not joined:
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
                joined = True
            data, addr = sock.recvfrom(65535)
        except socket.timeout:
            # re-join now and then: a WLAN reconnect can drop the membership
            try:
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_DROP_MEMBERSHIP, mreq)
            except OSError:
                pass
            joined = False
            continue
        except OSError as e:  # network not up yet at boot
            print(f"WS-Discovery: {e}, neuer Versuch in 10 s", flush=True)
            joined = False
            time.sleep(10)
            continue
        try:
            ctx = ctx_for_ip(local_ip_for(addr[0]))
            if not ctx["camera"]["enabled"]:
                continue
            reply = probe_match(data, ctx)
            if reply:
                sock.sendto(reply, addr)
        except OSError as e:
            print(f"WS-Discovery: {e}", flush=True)
            time.sleep(1)
