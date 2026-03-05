#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    WebSocket,
    AudioSocket,
    Udp,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::AudioSocket => "tcp",
            Self::Udp => "udp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "websocket" | "ws" => Some(Self::WebSocket),
            "tcp" | "audiosocket" => Some(Self::AudioSocket),
            "udp" | "rtp" => Some(Self::Udp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encapsulation {
    None,
    AudioSocket,
    Rtp,
}

impl Encapsulation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AudioSocket => "audiosocket",
            Self::Rtp => "rtp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "audiosocket" | "tcp" => Some(Self::AudioSocket),
            "rtp" | "udp" => Some(Self::Rtp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMediaRequest {
    pub app: String,
    pub external_host: String,
    pub format: String,
    pub transport: Transport,
    pub encapsulation: Encapsulation,
    pub connection_type: String,
    pub transport_data: Option<String>,
    pub direction: String,
    pub data: String,
}

impl ExternalMediaRequest {
    #[cfg(test)]
    pub fn recommended_websocket(app: impl Into<String>, external_host: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            external_host: external_host.into(),
            format: "ulaw".to_string(),
            transport: Transport::WebSocket,
            encapsulation: Encapsulation::None,
            connection_type: "server".to_string(),
            transport_data: Some("f(json)".to_string()),
            direction: "both".to_string(),
            data: "slmodem".to_string(),
        }
    }

    pub fn from_env_defaults() -> Self {
        let app = std::env::var("ARI_APP").unwrap_or_else(|_| "slmodem".to_string());
        let external_host =
            std::env::var("ARI_EXTERNAL_HOST").unwrap_or_else(|_| "INCOMING".to_string());
        let format = std::env::var("ARI_MEDIA_FORMAT").unwrap_or_else(|_| "ulaw".to_string());

        let transport = std::env::var("ARI_MEDIA_TRANSPORT")
            .ok()
            .and_then(|v| Transport::parse(&v))
            .unwrap_or(Transport::WebSocket);

        let encapsulation_default = match transport {
            Transport::WebSocket => Encapsulation::None,
            Transport::AudioSocket => Encapsulation::AudioSocket,
            Transport::Udp => Encapsulation::Rtp,
        };

        let encapsulation = std::env::var("ARI_MEDIA_ENCAPSULATION")
            .ok()
            .and_then(|v| Encapsulation::parse(&v))
            .unwrap_or(encapsulation_default);

        let connection_type =
            std::env::var("ARI_MEDIA_CONNECTION_TYPE").unwrap_or_else(|_| "server".to_string());
        let transport_data = std::env::var("ARI_MEDIA_TRANSPORT_DATA")
            .ok()
            .or_else(|| Some("f(json)".to_string()));
        let direction = std::env::var("ARI_MEDIA_DIRECTION").unwrap_or_else(|_| "both".to_string());
        let data = std::env::var("ARI_MEDIA_DATA").unwrap_or_else(|_| "slmodem".to_string());

        Self {
            app,
            external_host,
            format,
            transport,
            encapsulation,
            connection_type,
            transport_data,
            direction,
            data,
        }
    }

    pub fn query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("app".to_string(), self.app.clone()),
            ("external_host".to_string(), self.external_host.clone()),
            ("format".to_string(), self.format.clone()),
            ("transport".to_string(), self.transport.as_str().to_string()),
            (
                "encapsulation".to_string(),
                self.encapsulation.as_str().to_string(),
            ),
            ("connection_type".to_string(), self.connection_type.clone()),
            ("direction".to_string(), self.direction.clone()),
            ("data".to_string(), self.data.clone()),
        ];

        if let Some(transport_data) = &self.transport_data {
            pairs.push(("transport_data".to_string(), transport_data.clone()));
        }

        pairs
    }

    pub fn endpoint_path() -> &'static str {
        "/channels/externalMedia"
    }
}

#[cfg(test)]
mod tests {
    use super::{Encapsulation, ExternalMediaRequest, Transport};

    #[test]
    fn websocket_recommendation_matches_project_default() {
        let req = ExternalMediaRequest::recommended_websocket("slmodem", "media_conn");
        assert_eq!(req.transport, Transport::WebSocket);
        assert_eq!(req.encapsulation, Encapsulation::None);
        assert_eq!(req.format, "ulaw");
        assert_eq!(req.connection_type, "server");
        assert_eq!(req.transport_data.as_deref(), Some("f(json)"));
    }

    #[test]
    fn renders_expected_query_pairs() {
        let req = ExternalMediaRequest::recommended_websocket("slmodem", "media_conn");
        let pairs = req.query_pairs();

        assert!(pairs.contains(&("app".to_string(), "slmodem".to_string())));
        assert!(pairs.contains(&("external_host".to_string(), "media_conn".to_string())));
        assert!(pairs.contains(&("format".to_string(), "ulaw".to_string())));
        assert!(pairs.contains(&("transport".to_string(), "websocket".to_string())));
        assert!(pairs.contains(&("encapsulation".to_string(), "none".to_string())));
        assert!(pairs.contains(&("transport_data".to_string(), "f(json)".to_string())));
    }

    #[test]
    fn parse_transport_variants() {
        assert_eq!(Transport::parse("websocket"), Some(Transport::WebSocket));
        assert_eq!(
            Transport::parse("audiosocket"),
            Some(Transport::AudioSocket)
        );
        assert_eq!(Transport::parse("rtp"), Some(Transport::Udp));
    }

    #[test]
    fn parse_encapsulation_variants() {
        assert_eq!(Encapsulation::parse("none"), Some(Encapsulation::None));
        assert_eq!(
            Encapsulation::parse("audiosocket"),
            Some(Encapsulation::AudioSocket)
        );
        assert_eq!(Encapsulation::parse("rtp"), Some(Encapsulation::Rtp));
    }
}
