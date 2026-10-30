// hermes-platform-whatsapp - WhatsApp Cloud API adapter
use hermes_platform_adapter::{PlatformAdapter, BaseAdapter, BasePlatformConfig};
use hermes_protocol::{
    Platform, PlatformEvent, PlatformError, DeliveryReceipt, DeliveryStatus,
    IncomingMessage, OutgoingMessage, MessageId, MessageContent,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use reqwest::{Client, header::{AUTHORIZATION, CONTENT_TYPE}};
use tracing::{info, warn, error};
use uuid::Uuid;
use time::OffsetDateTime;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsAppConfig {
    pub access_token: String,
    pub phone_number_id: String,
    pub webhook_verify_token: String,
    pub webhook_url: Option<String>,
    pub app_secret: String,
    pub api_version: String,
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct WhatsAppUser {
    wa_id: String,
    profile: Option<WhatsAppProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppProfile {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppContact {
    profile: WhatsAppProfile,
    wa_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppMessage {
    from: String,
    id: String,
    timestamp: String,
    #[serde(rename = "type")]
    msg_type: String,
    text: Option<WhatsAppText>,
    image: Option<WhatsAppMedia>,
    document: Option<WhatsAppMedia>,
    audio: Option<WhatsAppMedia>,
    video: Option<WhatsAppMedia>,
    location: Option<WhatsAppLocation>,
    contacts: Option<Vec<WhatsAppContactMessage>>,
    template: Option<WhatsAppTemplate>,
    context: Option<WhatsAppContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppText {
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppMedia {
    id: String,
    mime_type: Option<String>,
    sha256: Option<String>,
    caption: Option<String>,
    filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppLocation {
    latitude: f64,
    longitude: f64,
    name: Option<String>,
    address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppContactMessage {
    name: WhatsAppContactName,
    phones: Option<Vec<WhatsAppPhone>>,
    emails: Option<Vec<WhatsAppEmail>>,
    addresses: Option<Vec<WhatsAppAddress>>,
    org: Option<WhatsAppOrg>,
    urls: Option<Vec<WhatsAppUrl>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppContactName {
    formatted_name: String,
    first_name: Option<String>,
    last_name: Option<String>,
    middle_name: Option<String>,
    suffix: Option<String>,
    prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppPhone {
    phone: String,
    #[serde(rename = "type")]
    phone_type: Option<String>,
    wa_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppEmail {
    email: String,
    #[serde(rename = "type")]
    email_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppAddress {
    street: Option<String>,
    city: Option<String>,
    state: Option<String>,
    zip: Option<String>,
    country: Option<String>,
    country_code: Option<String>,
    #[serde(rename = "type")]
    address_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppOrg {
    company: Option<String>,
    department: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppUrl {
    url: String,
    #[serde(rename = "type")]
    url_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppTemplate {
    name: String,
    language: WhatsAppTemplateLanguage,
    components: Option<Vec<WhatsAppTemplateComponent>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppTemplateLanguage {
    code: String,
    policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppTemplateComponent {
    #[serde(rename = "type")]
    component_type: String,
    text: Option<String>,
    parameters: Option<Vec<WhatsAppTemplateParameter>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppTemplateParameter {
    #[serde(rename = "type")]
    param_type: String,
    text: Option<String>,
    currency: Option<WhatsAppCurrency>,
    date_time: Option<String>,
    image: Option<WhatsAppMediaRef>,
    document: Option<WhatsAppMediaRef>,
    video: Option<WhatsAppMediaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppCurrency {
    fallback_value: String,
    code: String,
    amount_1000: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppMediaRef {
    id: Option<String>,
    link: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppContext {
    from: String,
    id: String,
    referred_product: Option<WhatsAppReferredProduct>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppReferredProduct {
    catalog_id: String,
    product_retailer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppStatus {
    id: String,
    status: String,
    timestamp: String,
    recipient_id: String,
    conversation: Option<WhatsAppConversation>,
    pricing: Option<WhatsAppPricing>,
    errors: Option<Vec<WhatsAppError>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppConversation {
    id: String,
    origin: Option<WhatsAppConversationOrigin>,
    expiration_timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppConversationOrigin {
    #[serde(rename = "type")]
    origin_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppPricing {
    billable: bool,
    pricing_model: String,
    category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppError {
    code: i32,
    title: String,
    message: String,
    error_data: Option<WhatsAppErrorData>,
    error_subcode: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppErrorData {
    details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppValue {
    messaging_product: String,
    metadata: WhatsAppMetadata,
    contacts: Option<Vec<WhatsAppContact>>,
    messages: Option<Vec<WhatsAppMessage>>,
    statuses: Option<Vec<WhatsAppStatus>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppMetadata {
    display_phone_number: String,
    phone_number_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppEntry {
    id: String,
    changes: Vec<WhatsAppChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppChange {
    value: WhatsAppValue,
    field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WhatsAppWebhookPayload {
    object: String,
    entry: Vec<WhatsAppEntry>,
}

#[derive(Debug, Serialize)]
struct SendTextRequest {
    messaging_product: String,
    to: String,
    #[serde(rename = "type")]
    msg_type: String,
    text: WhatsAppText,
    context: Option<WhatsAppContextRef>,
}

#[derive(Debug, Serialize)]
struct SendMediaRequest {
    messaging_product: String,
    to: String,
    #[serde(rename = "type")]
    msg_type: String,
    image: Option<WhatsAppMediaRef>,
    document: Option<WhatsAppMediaRef>,
    audio: Option<WhatsAppMediaRef>,
    video: Option<WhatsAppVideoRef>,
    caption: Option<String>,
    context: Option<WhatsAppContextRef>,
}

#[derive(Debug, Serialize)]
struct WhatsAppVideoRef {
    id: Option<String>,
    link: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendLocationRequest {
    messaging_product: String,
    to: String,
    #[serde(rename = "type")]
    msg_type: String,
    location: WhatsAppLocation,
    context: Option<WhatsAppContextRef>,
}

#[derive(Debug, Serialize)]
struct SendContactsRequest {
    messaging_product: String,
    to: String,
    #[serde(rename = "type")]
    msg_type: String,
    contacts: Vec<WhatsAppContactMessage>,
    context: Option<WhatsAppContextRef>,
}

#[derive(Debug, Serialize)]
struct SendTemplateRequest {
    messaging_product: String,
    to: String,
    #[serde(rename = "type")]
    msg_type: String,
    template: WhatsAppTemplate,
    context: Option<WhatsAppContextRef>,
}

#[derive(Debug, Serialize)]
struct WhatsAppContextRef {
    message_id: String,
}

// Media upload request - Part cannot be serialized, so we don't derive Serialize
#[derive(Debug)]
#[allow(dead_code)]
struct MediaUploadRequest {
    messaging_product: String,
    // msg_type is handled separately in multipart form
    // file is handled separately in multipart form
}

#[derive(Debug, Deserialize)]
struct MediaUploadResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MediaUrlResponse {
    url: String,
    mime_type: String,
    sha256: String,
    file_size: i64,
    id: String,
}

#[derive(Debug, Deserialize)]
struct WhatsAppApiResponse {
    #[allow(dead_code)]
    messaging_product: String,
    contacts: Option<Vec<WhatsAppContactResponse>>,
    messages: Option<Vec<WhatsAppMessageResponse>>,
    error: Option<WhatsAppError>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WhatsAppContactResponse {
    input: String,
    status: String,
    wa_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct WhatsAppMessageResponse {
    id: String,
}

#[derive(Debug)]
pub struct WhatsAppAdapter {
    base: BaseAdapter<WhatsAppConfig>,
    client: Client,
    session_to_wa: Arc<dashmap::DashMap<Uuid, String>>,
    #[allow(dead_code)]
    pending_messages: Arc<RwLock<HashMap<MessageId, (OutgoingMessage, mpsc::UnboundedSender<DeliveryReceipt>)>>>,
}

impl WhatsAppAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::WhatsApp),
            client: Client::new(),
            pending_messages: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn api_base_url(&self) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        format!("https://graph.facebook.com/{}/{}", config.api_version, config.phone_number_id)
    }

    fn media_url(&self) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        format!("https://graph.facebook.com/{}/{}", config.api_version, config.phone_number_id)
    }

    fn auth_header(&self) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        format!("Bearer {}", config.access_token)
    }

    async fn make_request<T: for<'de> Deserialize<'de>>(&self, endpoint: &str, body: &impl Serialize) -> Result<T, PlatformError> {
        let url = format!("{}/{}", self.api_base_url(), endpoint);
        let _config = self.base.config.as_ref().expect("Not initialized");

        let response = self.client
            .post(&url)
            .header(AUTHORIZATION, self.auth_header())
            .header(CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let api_response: WhatsAppApiResponse = response.json().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !status.is_success() {
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: "WhatsApp API request failed".to_string(),
            });
        }

        if let Some(error) = api_response.error {
            return Err(PlatformError::Api {
                code: error.code.to_string(),
                message: error.message,
            });
        }

        // Try to extract from messages array first, then contacts
        if let Some(messages) = api_response.messages {
            if let Some(first) = messages.first() {
                return Ok(serde_json::from_value(serde_json::to_value(first).unwrap()).unwrap());
            }
        }
        if let Some(contacts) = api_response.contacts {
            if let Some(first) = contacts.first() {
                return Ok(serde_json::from_value(serde_json::to_value(first).unwrap()).unwrap());
            }
        }

        Err(PlatformError::Internal("Empty result from WhatsApp API".into()))
    }

    async fn upload_media(&self, media_type: &str, file_data: bytes::Bytes, mime_type: &str, filename: Option<&str>) -> Result<String, PlatformError> {
        let url = format!("{}/media", self.media_url());
        let _config = self.base.config.as_ref().expect("Not initialized");

        let file_name = filename.unwrap_or(match media_type {
            "image" => "image.jpg",
            "document" => "document.pdf",
            "audio" => "audio.ogg",
            "video" => "video.mp4",
            _ => "file.bin",
        });

        let part = reqwest::multipart::Part::bytes(file_data.to_vec())
            .file_name(file_name.to_string())
            .mime_str(mime_type)
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        let media_type_owned = media_type.to_string();
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .text("type", media_type_owned)
            .part("file", part);

        let response = self.client
            .post(&url)
            .header(AUTHORIZATION, self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let upload_response: MediaUploadResponse = response.json().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !status.is_success() {
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: format!("Media upload failed: {:?}", upload_response),
            });
        }

        Ok(upload_response.id)
    }

    async fn get_media_url(&self, media_id: &str) -> Result<String, PlatformError> {
        let url = format!("{}/{}", self.media_url(), media_id);
        let _config = self.base.config.as_ref().expect("Not initialized");

        let response = self.client
            .get(&url)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let media_response: MediaUrlResponse = response.json().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !status.is_success() {
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: "Failed to get media URL".to_string(),
            });
        }

        Ok(media_response.url)
    }

    async fn download_media(&self, media_url: &str) -> Result<bytes::Bytes, PlatformError> {
        let _config = self.base.config.as_ref().expect("Not initialized");

        let response = self.client
            .get(media_url)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: "Failed to download media".to_string(),
            });
        }

        let bytes = response.bytes().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        Ok(bytes)
    }

    #[allow(dead_code)]
    fn verify_webhook_signature(&self, payload: &[u8], signature: &str) -> bool {
        let config = self.base.config.as_ref().expect("Not initialized");
        
        let mut mac = Hmac::<Sha256>::new_from_slice(config.app_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(payload);
        let result = mac.finalize();
        let expected_signature = hex::encode(result.into_bytes());
        
        // WhatsApp sends signature as "sha256=<hex>"
        signature == format!("sha256={}", expected_signature)
    }

    fn session_id_from_wa_id(&self, wa_id: &str) -> Uuid {
        // Deterministic UUID from WhatsApp ID
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        wa_id.hash(&mut hasher);
        let hash = hasher.finish();
        let bytes = hash.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&bytes);
        Uuid::from_bytes(uuid_bytes)
    }

    fn convert_incoming_message(&self, msg: &WhatsAppMessage, _metadata: &WhatsAppMetadata) -> Result<IncomingMessage, PlatformError> {
        let session_id = self.session_id_from_wa_id(&msg.from);
        self.session_to_wa.insert(session_id, msg.from.clone());
        let content = self.convert_message_content(msg)?;
        
        let timestamp = msg.timestamp.parse::<i64>()
            .map(|ts| OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| OffsetDateTime::now_utc()))
            .unwrap_or_else(|_| OffsetDateTime::now_utc());

        Ok(IncomingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::WhatsApp,
            content,
            timestamp,
            raw: Some(serde_json::to_value(msg).unwrap_or(serde_json::Value::Null)),
        })
    }

    fn convert_message_content(&self, msg: &WhatsAppMessage) -> Result<MessageContent, PlatformError> {
        match msg.msg_type.as_str() {
            "text" => {
                if let Some(text) = &msg.text {
                    return Ok(MessageContent::Text { text: text.body.clone() });
                }
            }
            "image" => {
                if let Some(image) = &msg.image {
                    return Ok(MessageContent::Image {
                        url: image.id.clone(),
                        caption: image.caption.clone(),
                    });
                }
            }
            "document" => {
                if let Some(doc) = &msg.document {
                    return Ok(MessageContent::File {
                        url: doc.id.clone(),
                        filename: doc.filename.clone().unwrap_or_else(|| "document".to_string()),
                        mime_type: doc.mime_type.clone(),
                    });
                }
            }
            "audio" => {
                if let Some(audio) = &msg.audio {
                    return Ok(MessageContent::Audio {
                        url: audio.id.clone(),
                        duration: None, // WhatsApp doesn't provide duration in webhook
                    });
                }
            }
            "video" => {
                if let Some(video) = &msg.video {
                    return Ok(MessageContent::Video {
                        url: video.id.clone(),
                        duration: None,
                    });
                }
            }
            "location" => {
                if let Some(loc) = &msg.location {
                    return Ok(MessageContent::Location {
                        latitude: loc.latitude,
                        longitude: loc.longitude,
                    });
                }
            }
            "contacts" => {
                if let Some(contacts) = &msg.contacts {
                    if let Some(contact) = contacts.first() {
                        let name = contact.name.formatted_name.clone();
                        let phone = contact.phones.as_ref()
                            .and_then(|p| p.first())
                            .map(|p| p.phone.clone())
                            .unwrap_or_default();
                        return Ok(MessageContent::Contact { name, phone });
                    }
                }
            }
            "template" => {
                // Template messages are inbound when user replies to a template
                if let Some(template) = &msg.template {
                    return Ok(MessageContent::System {
                        event: "template_reply".to_string(),
                        data: serde_json::to_value(template).unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }

        Ok(MessageContent::Unknown { data: serde_json::to_value(msg).unwrap_or_default() })
    }

    fn convert_outgoing_message(&self, msg: &OutgoingMessage) -> Result<serde_json::Value, PlatformError> {
        let _config = self.base.config.as_ref().expect("Not initialized");
        let to = self.wa_id_from_session(msg.session_id)?;

        match &msg.content {
            MessageContent::Text { text } => {
                let req = SendTextRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "text".to_string(),
                    text: WhatsAppText { body: text.clone() },
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Image { url, caption } => {
                let req = SendMediaRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "image".to_string(),
                    image: Some(WhatsAppMediaRef { id: Some(url.clone()), link: None }),
                    document: None,
                    audio: None,
                    video: None,
                    caption: caption.clone(),
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::File { url, filename, mime_type: _ } => {
                let req = SendMediaRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "document".to_string(),
                    image: None,
                    document: Some(WhatsAppMediaRef { id: Some(url.clone()), link: None }),
                    audio: None,
                    video: None,
                    caption: Some(filename.clone()),
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Audio { url, duration: _ } => {
                let req = SendMediaRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "audio".to_string(),
                    image: None,
                    document: None,
                    audio: Some(WhatsAppMediaRef { id: Some(url.clone()), link: None }),
                    video: None,
                    caption: None,
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Video { url, duration: _ } => {
                let req = SendMediaRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "video".to_string(),
                    image: None,
                    document: None,
                    audio: None,
                    video: Some(WhatsAppVideoRef { id: Some(url.clone()), link: None }),
                    caption: None,
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Location { latitude, longitude } => {
                let req = SendLocationRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "location".to_string(),
                    location: WhatsAppLocation {
                        latitude: *latitude,
                        longitude: *longitude,
                        name: None,
                        address: None,
                    },
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Contact { name, phone } => {
                let req = SendContactsRequest {
                    messaging_product: "whatsapp".to_string(),
                    to,
                    msg_type: "contacts".to_string(),
                    contacts: vec![WhatsAppContactMessage {
                        name: WhatsAppContactName {
                            formatted_name: name.clone(),
                            first_name: name.split_whitespace().next().map(|s| s.to_string()),
                            last_name: name.split_whitespace().last().map(|s| s.to_string()),
                            middle_name: None,
                            suffix: None,
                            prefix: None,
                        },
                        phones: Some(vec![WhatsAppPhone {
                            phone: phone.clone(),
                            phone_type: Some("MOBILE".to_string()),
                            wa_id: None,
                        }]),
                        emails: None,
                        addresses: None,
                        org: None,
                        urls: None,
                    }],
                    context: msg.reply_to.map(|id| WhatsAppContextRef { 
                        message_id: id.0.to_string() 
                    }),
                };
                serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::System { event, data } if event == "template" => {
                if let Some(template) = data.get("template") {
                    let template: WhatsAppTemplate = serde_json::from_value(template.clone())
                        .map_err(|e| PlatformError::Internal(e.to_string()))?;
                    let req = SendTemplateRequest {
                        messaging_product: "whatsapp".to_string(),
                        to,
                        msg_type: "template".to_string(),
                        template,
                        context: msg.reply_to.map(|id| WhatsAppContextRef { 
                            message_id: id.0.to_string() 
                        }),
                    };
                    serde_json::to_value(req).map_err(|e| PlatformError::Internal(e.to_string()))
                } else {
                    Err(PlatformError::UnsupportedContent("Template data missing".into()))
                }
            }
            _ => Err(PlatformError::UnsupportedContent(format!("Content type not supported for WhatsApp: {:?}", msg.content))),
        }
    }

    fn wa_id_from_session(&self, session_id: Uuid) -> Result<String, PlatformError> {
        if let Some(wa_id) = self.session_to_wa.get(&session_id) {
            return Ok(wa_id.clone());
        }
        let bytes = session_id.as_bytes();
        let hash = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| PlatformError::Internal("Invalid session ID".into()))?);
        Ok(format!("wa_id_{:x}", hash))
    }

    // Public methods for media handling
    pub async fn upload_media_file(&self, media_type: &str, file_data: bytes::Bytes, mime_type: &str, filename: Option<&str>) -> Result<String, PlatformError> {
        self.upload_media(media_type, file_data, mime_type, filename).await
    }

    pub async fn get_media_download_url(&self, media_id: &str) -> Result<String, PlatformError> {
        self.get_media_url(media_id).await
    }

    pub async fn download_media_file(&self, media_url: &str) -> Result<bytes::Bytes, PlatformError> {
        self.download_media(media_url).await
    }
}

#[async_trait]
impl PlatformAdapter for WhatsAppAdapter {
    type Config = WhatsAppConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::WhatsApp
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        self.base = BaseAdapter::new(Platform::WhatsApp).with_config(config.clone());

        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }

        // Verify credentials by calling the phone number info endpoint
        let url = format!("{}/phone_numbers", self.api_base_url().replace(&format!("/{}", config.phone_number_id), ""));
        let response = self.client
            .get(&url)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(PlatformError::Auth("Invalid access token or phone number ID".into()));
        }

        // Set webhook if configured
        if let Some(webhook_url) = &config.webhook_url {
            let webhook_config = config.base.webhook.clone();
            let app_config_url = format!("{}/webhook", self.api_base_url().replace(&format!("/{}", config.phone_number_id), "").replace("/phone_numbers", ""));
            
            let mut params = serde_json::json!({
                "url": webhook_url,
                "verify_token": config.webhook_verify_token,
                "fields": ["messages", "message_deliveries", "message_reads", "message_template_status_updates"]
            });
            
            if let Some(secret) = webhook_config.and_then(|w| w.secret) {
                params["app_secret"] = serde_json::Value::String(secret);
            }

            let response = self.client
                .post(&app_config_url)
                .header(AUTHORIZATION, self.auth_header())
                .json(&params)
                .send()
                .await
                .map_err(|e| PlatformError::Network(e.to_string()))?;

            if response.status().is_success() {
                info!("WhatsApp webhook configured: {}", webhook_url);
            } else {
                warn!("Failed to set WhatsApp webhook: {}", response.status());
            }
        }

        info!("WhatsApp adapter initialized for phone_number_id: {}", config.phone_number_id);
        Ok(())
    }

    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        self.base.set_sender(sender);
        self.base.started = true;
        info!("WhatsApp adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;
        info!("WhatsApp adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }

        let payload = self.convert_outgoing_message(&msg)?;
        let response: WhatsAppMessageResponse = self.make_request("messages", &payload).await?;

        Ok(DeliveryReceipt {
            message_id: msg.id,
            platform: Platform::WhatsApp,
            status: DeliveryStatus::Sent,
            timestamp: OffsetDateTime::now_utc(),
            platform_message_id: Some(response.id),
        })
    }

    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        let webhook: WhatsAppWebhookPayload = serde_json::from_value(payload)
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        let mut messages = Vec::new();

        for entry in webhook.entry {
            for change in entry.changes {
                let value = change.value;
                
                // Handle incoming messages
                if let Some(msgs) = value.messages {
                    for msg in msgs {
                        match self.convert_incoming_message(&msg, &value.metadata) {
                            Ok(incoming) => messages.push(incoming),
                            Err(e) => {
                                error!("Failed to convert incoming WhatsApp message: {}", e);
                            }
                        }
                    }
                }

                // Handle status updates
                if let Some(statuses) = value.statuses {
                    for status in statuses {
                        if let Some(sender) = &self.base.sender {
                            let delivery_status = match status.status.as_str() {
                                "sent" => DeliveryStatus::Sent,
                                "delivered" => DeliveryStatus::Delivered,
                                "read" => DeliveryStatus::Read,
                                "failed" => DeliveryStatus::Failed,
                                _ => DeliveryStatus::Pending,
                            };
                            
                            let _ = sender.send(PlatformEvent::Ack {
                                message_id: MessageId(Uuid::parse_str(&status.id).unwrap_or_else(|_| Uuid::now_v7())),
                                success: delivery_status != DeliveryStatus::Failed,
                            });
                        }
                    }
                }
            }
        }

        Ok(messages)
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        let config = self.base.config.as_ref().expect("Not initialized");
        let url = format!("{}/phone_numbers", self.api_base_url().replace(&format!("/{}", config.phone_number_id), ""));
        let response = self.client
            .get(&url)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(PlatformError::Api {
                code: response.status().to_string(),
                message: "Health check failed".to_string(),
            })
        }
    }
}

impl Default for WhatsAppAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// Webhook verification handler for the HTTP layer
pub async fn verify_webhook(
    query: &std::collections::HashMap<String, String>,
    verify_token: &str,
) -> Option<String> {
    let mode = query.get("hub.mode")?;
    let token = query.get("hub.verify_token")?;
    let challenge = query.get("hub.challenge")?;

    if mode == "subscribe" && token == verify_token {
        Some(challenge.clone())
    } else {
        None
    }
}

pub async fn verify_webhook_signature(
    payload: &[u8],
    signature_header: Option<&str>,
    app_secret: &str,
) -> Result<bool, PlatformError> {
    let signature = signature_header.ok_or_else(|| PlatformError::Internal("Missing signature header".into()))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(payload);
    let result = mac.finalize();
    let expected_signature = hex::encode(result.into_bytes());
    Ok(signature == format!("sha256={}", expected_signature))
}