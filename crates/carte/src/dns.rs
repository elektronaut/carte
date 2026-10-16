use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, SOA};
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const TTL: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("invalid TLD {tld:?}: {reason}")]
    InvalidTld { tld: String, reason: String },
    #[error("no TLDs configured")]
    NoTlds,
}

pub fn resolver_file_path(tld: &str) -> PathBuf {
    PathBuf::from("/etc/resolver").join(tld)
}

pub fn resolver_file_contents(port: u16) -> String {
    format!("nameserver 127.0.0.1\nport {port}\n")
}

/// Answers queries for names at or below the configured TLDs with loopback addresses.
#[derive(Debug, Clone)]
pub struct Responder {
    zones: Vec<Zone>,
}

#[derive(Debug, Clone)]
struct Zone {
    name: Name,
    soa: Record,
}

impl Responder {
    pub fn new<S: AsRef<str>>(tlds: &[S]) -> Result<Self, DnsError> {
        if tlds.is_empty() {
            return Err(DnsError::NoTlds);
        }
        let zones = tlds
            .iter()
            .map(|tld| Zone::new(tld.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(Self { zones })
    }

    /// Returns the encoded response, or `None` when the packet should be dropped.
    pub fn respond(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let response = match Message::from_vec(packet) {
            Ok(request) if request.metadata.message_type == MessageType::Query => {
                self.answer(&request)
            }
            Ok(_) => return None,
            Err(_) => {
                let id = u16::from_be_bytes([*packet.first()?, *packet.get(1)?]);
                Message::error_msg(id, OpCode::Query, ResponseCode::FormErr)
            }
        };
        response.to_vec().ok()
    }

    fn answer(&self, request: &Message) -> Message {
        let mut response = Message::response(request.metadata.id, request.metadata.op_code);
        response.metadata.recursion_desired = request.metadata.recursion_desired;
        response.add_queries(request.queries.iter().cloned());
        if request.edns.is_some() {
            let mut edns = Edns::new();
            edns.set_max_payload(1232);
            response.set_edns(edns);
        }

        let query = match request.queries.as_slice() {
            _ if request.metadata.op_code != OpCode::Query => {
                response.metadata.response_code = ResponseCode::NotImp;
                return response;
            }
            [query] => query,
            _ => {
                response.metadata.response_code = ResponseCode::FormErr;
                return response;
            }
        };

        let in_class = matches!(query.query_class, DNSClass::IN | DNSClass::ANY);
        let zone = self
            .zones
            .iter()
            .find(|zone| zone.name.zone_of(&query.name));
        let Some(zone) = zone.filter(|_| in_class) else {
            response.metadata.response_code = ResponseCode::Refused;
            return response;
        };

        response.metadata.authoritative = true;
        let name = query.name.clone();
        let is_apex = zone.name.eq_ignore_root(&query.name);
        match query.query_type {
            RecordType::A => {
                response.add_answer(Record::from_rdata(
                    name,
                    TTL,
                    RData::A(A(Ipv4Addr::LOCALHOST)),
                ));
            }
            RecordType::AAAA => {
                response.add_answer(Record::from_rdata(
                    name,
                    TTL,
                    RData::AAAA(AAAA(Ipv6Addr::LOCALHOST)),
                ));
            }
            RecordType::ANY => {
                response.add_answer(Record::from_rdata(
                    name.clone(),
                    TTL,
                    RData::A(A(Ipv4Addr::LOCALHOST)),
                ));
                response.add_answer(Record::from_rdata(
                    name,
                    TTL,
                    RData::AAAA(AAAA(Ipv6Addr::LOCALHOST)),
                ));
            }
            RecordType::SOA if is_apex => {
                response.add_answer(zone.soa.clone());
            }
            _ => {
                response.add_authority(zone.soa.clone());
            }
        }
        response
    }
}

impl Zone {
    fn new(tld: &str) -> Result<Self, DnsError> {
        let invalid = |reason: String| DnsError::InvalidTld {
            tld: tld.to_owned(),
            reason,
        };
        let trimmed = tld.trim_end_matches('.');
        if trimmed.is_empty() {
            return Err(invalid("empty".into()));
        }
        let mut name = Name::from_ascii(trimmed).map_err(|e| invalid(e.to_string()))?;
        name.set_fqdn(true);
        let name = name.to_lowercase();
        let rname = Name::from_ascii("hostmaster")
            .and_then(|label| label.append_domain(&name))
            .map_err(|e| invalid(e.to_string()))?;
        let soa = SOA::new(name.clone(), rname, 1, 3600, 600, 86400, TTL);
        let soa = Record::from_rdata(name.clone(), TTL, RData::SOA(soa));
        Ok(Self { name, soa })
    }
}

/// A DNS server listening on UDP and TCP on the same address.
pub struct DnsServer {
    udp: UdpSocket,
    tcp: TcpListener,
    responder: Arc<Responder>,
}

impl DnsServer {
    /// Binds UDP and TCP. With port 0, both share the port picked for TCP.
    pub async fn bind(addr: SocketAddr, responder: Responder) -> io::Result<Self> {
        let tcp = TcpListener::bind(addr).await?;
        let udp = UdpSocket::bind(tcp.local_addr()?).await?;
        Ok(Self {
            udp,
            tcp,
            responder: Arc::new(responder),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// Serves until the future is dropped.
    pub async fn run(self) {
        tokio::join!(
            serve_udp(self.udp, self.responder.clone()),
            serve_tcp(self.tcp, self.responder),
        );
    }
}

async fn serve_udp(socket: UdpSocket, responder: Arc<Responder>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                tracing::debug!(error = %e, "dns: udp receive failed");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        if let Some(response) = responder.respond(&buf[..len])
            && let Err(e) = socket.send_to(&response, peer).await
        {
            tracing::debug!(%peer, error = %e, "dns: udp send failed");
        }
    }
}

async fn serve_tcp(listener: TcpListener, responder: Arc<Responder>) {
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "dns: tcp accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        while tasks.try_join_next().is_some() {}
        let responder = responder.clone();
        tasks.spawn(async move {
            if let Err(e) = serve_tcp_connection(stream, &responder).await {
                tracing::debug!(%peer, error = %e, "dns: tcp connection failed");
            }
        });
    }
}

async fn serve_tcp_connection(mut stream: TcpStream, responder: &Responder) -> io::Result<()> {
    let idle = Duration::from_secs(10);
    loop {
        let len = match tokio::time::timeout(idle, stream.read_u16()).await {
            Ok(Ok(len)) => usize::from(len),
            Ok(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok(()),
        };
        let mut packet = vec![0u8; len];
        tokio::time::timeout(idle, stream.read_exact(&mut packet))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        let Some(response) = responder.respond(&packet) else {
            return Ok(());
        };
        let len = u16::try_from(response.len()).map_err(|_| io::ErrorKind::InvalidData)?;
        let mut framed = Vec::with_capacity(response.len() + 2);
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&response);
        stream.write_all(&framed).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;

    fn query(name: &str, record_type: RecordType) -> Message {
        let mut message = Message::query();
        message.metadata.recursion_desired = true;
        message.add_query(Query::query(Name::from_ascii(name).unwrap(), record_type));
        message
    }

    fn ask(responder: &Responder, request: &Message) -> Message {
        let packet = responder.respond(&request.to_vec().unwrap()).unwrap();
        Message::from_vec(&packet).unwrap()
    }

    fn responder() -> Responder {
        Responder::new(&["test"]).unwrap()
    }

    #[test]
    fn answers_a_for_names_at_any_depth() {
        for name in [
            "test.",
            "app.test.",
            "oslo.feature-x.delegatbase.test.",
            "a.b.c.d.e.test",
        ] {
            let request = query(name, RecordType::A);
            let response = ask(&responder(), &request);
            assert_eq!(
                response.metadata.response_code,
                ResponseCode::NoError,
                "{name}"
            );
            assert_eq!(response.metadata.id, request.metadata.id);
            assert!(response.metadata.authoritative);
            assert!(response.metadata.recursion_desired);
            assert_eq!(response.answers.len(), 1);
            assert_eq!(response.answers[0].data, RData::A(A(Ipv4Addr::LOCALHOST)));
            assert_eq!(response.answers[0].ttl, TTL);
        }
    }

    #[test]
    fn answers_aaaa_with_ipv6_loopback() {
        let response = ask(&responder(), &query("app.make.test.", RecordType::AAAA));
        assert_eq!(
            response.answers[0].data,
            RData::AAAA(AAAA(Ipv6Addr::LOCALHOST))
        );
    }

    #[test]
    fn matches_case_insensitively_and_echoes_the_query_name() {
        let response = ask(&responder(), &query("App.Make.TEST.", RecordType::A));
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert_eq!(response.answers[0].name.to_string(), "App.Make.TEST.");
        assert_eq!(response.queries[0].name.to_string(), "App.Make.TEST.");

        let responder = Responder::new(&["Dev."]).unwrap();
        let response = ask(&responder, &query("app.dev.", RecordType::A));
        assert_eq!(response.answers.len(), 1);
    }

    #[test]
    fn answers_nodata_with_soa_for_other_types() {
        for record_type in [
            RecordType::MX,
            RecordType::TXT,
            RecordType::HTTPS,
            RecordType::NS,
        ] {
            let response = ask(&responder(), &query("app.make.test.", record_type));
            assert_eq!(response.metadata.response_code, ResponseCode::NoError);
            assert!(response.answers.is_empty());
            assert_eq!(response.authorities.len(), 1);
            assert_eq!(response.authorities[0].record_type(), RecordType::SOA);
            assert_eq!(response.authorities[0].name.to_string(), "test.");
        }
    }

    #[test]
    fn answers_soa_at_the_apex() {
        let response = ask(&responder(), &query("test.", RecordType::SOA));
        assert_eq!(response.answers[0].record_type(), RecordType::SOA);
        let response = ask(&responder(), &query("app.test.", RecordType::SOA));
        assert!(response.answers.is_empty());
        assert_eq!(response.authorities.len(), 1);
    }

    #[test]
    fn refuses_names_outside_the_tlds() {
        for name in ["example.com.", "test.com.", "nottest.", "."] {
            let response = ask(&responder(), &query(name, RecordType::A));
            assert_eq!(
                response.metadata.response_code,
                ResponseCode::Refused,
                "{name}"
            );
            assert!(response.answers.is_empty());
        }
    }

    #[test]
    fn serves_several_tlds() {
        let responder = Responder::new(&["test", "localhost"]).unwrap();
        for name in ["a.test.", "a.localhost."] {
            let response = ask(&responder, &query(name, RecordType::A));
            assert_eq!(response.answers.len(), 1, "{name}");
        }
    }

    #[test]
    fn rejects_malformed_and_non_query_messages() {
        let responder = responder();
        let response = Message::from_vec(&responder.respond(&[0x12, 0x34, 0xff]).unwrap()).unwrap();
        assert_eq!(response.metadata.id, 0x1234);
        assert_eq!(response.metadata.response_code, ResponseCode::FormErr);
        assert!(responder.respond(&[0x12]).is_none());

        let mut reply = query("app.test.", RecordType::A);
        reply.metadata.message_type = MessageType::Response;
        assert!(responder.respond(&reply.to_vec().unwrap()).is_none());

        let mut update = query("app.test.", RecordType::A);
        update.metadata.op_code = OpCode::Update;
        assert_eq!(
            ask(&responder, &update).metadata.response_code,
            ResponseCode::NotImp
        );
    }

    #[test]
    fn echoes_edns() {
        let mut request = query("app.test.", RecordType::A);
        request.set_edns(Edns::new());
        assert!(ask(&responder(), &request).edns.is_some());
        assert!(
            ask(&responder(), &query("app.test.", RecordType::A))
                .edns
                .is_none()
        );
    }

    #[test]
    fn rejects_invalid_tlds() {
        assert!(Responder::new::<&str>(&[]).is_err());
        assert!(Responder::new(&[""]).is_err());
        assert!(Responder::new(&["."]).is_err());
    }

    #[test]
    fn renders_the_macos_resolver_file() {
        assert_eq!(
            resolver_file_path("test"),
            PathBuf::from("/etc/resolver/test")
        );
        assert_eq!(
            resolver_file_contents(5300),
            "nameserver 127.0.0.1\nport 5300\n"
        );
    }

    async fn server() -> SocketAddr {
        let server = DnsServer::bind("127.0.0.1:0".parse().unwrap(), responder())
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(server.run());
        addr
    }

    #[tokio::test]
    async fn serves_over_udp() {
        let addr = server().await;
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for (name, code) in [
            ("app.make.test.", ResponseCode::NoError),
            ("example.com.", ResponseCode::Refused),
        ] {
            let request = query(name, RecordType::A);
            socket
                .send_to(&request.to_vec().unwrap(), addr)
                .await
                .unwrap();
            let mut buf = [0u8; 512];
            let len = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            let response = Message::from_vec(&buf[..len]).unwrap();
            assert_eq!(response.metadata.id, request.metadata.id);
            assert_eq!(response.metadata.response_code, code);
        }
    }

    #[tokio::test]
    async fn serves_several_queries_over_one_tcp_connection() {
        let addr = server().await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        for (name, record_type) in [
            ("app.make.test.", RecordType::A),
            ("oslo.feature-x.delegatbase.test.", RecordType::AAAA),
        ] {
            let packet = query(name, record_type).to_vec().unwrap();
            stream.write_u16(packet.len() as u16).await.unwrap();
            stream.write_all(&packet).await.unwrap();
            let len = stream.read_u16().await.unwrap();
            let mut buf = vec![0u8; usize::from(len)];
            stream.read_exact(&mut buf).await.unwrap();
            let response = Message::from_vec(&buf).unwrap();
            assert_eq!(response.answers.len(), 1);
            assert_eq!(response.answers[0].record_type(), record_type);
        }
    }
}
